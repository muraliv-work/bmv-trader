//! The desk's store (SQLite via sqlx; plain SQL so the schema ports to Postgres). Every write runs
//! its checks (permission, state legality, evidence, maker-checker, budget) and appends an audit
//! entry in the same transaction, so nothing changes without a record (S30).
//!
//! shortcut: only the tables build step 1 needs; orders, fills, positions, reconciliations,
//! tickets, incidents and reports arrive with the steps that use them.
//! shortcut: the audit chain detects edits, but someone who can write the database file could
//! rewrite it whole; anchoring the head hash outside the file and running the desk as its own
//! macOS user come with the Desk API (build step 2).

use crate::audit::{self, Entry};
use crate::lifecycle::{self, HaltReason, Kind, PaperEvidence, Signature, State};
use crate::permissions::{self, Action, HumanProof, Role};
use crate::{DeskError, Result};
use rust_decimal::Decimal;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Sqlite, Transaction};
use std::str::FromStr;

/// Tables whose rows are history: never updated, never deleted.
const APPEND_ONLY: [&str; 8] = [
    "audit_log",
    "strategy_versions",
    "backtest_runs",
    "state_transitions",
    "approvals",
    "halts",
    "risk_limits",
    "paper_stats",
];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agents (
  id TEXT PRIMARY KEY, role TEXT NOT NULL, created TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS strategies (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, state TEXT NOT NULL, version INTEGER NOT NULL,
  hypothesis_author TEXT NOT NULL REFERENCES agents(id), spec_author TEXT REFERENCES agents(id),
  criteria TEXT, criteria_hash TEXT, current_halt INTEGER);
CREATE TABLE IF NOT EXISTS strategy_versions (
  strategy_id INTEGER NOT NULL REFERENCES strategies(id), version INTEGER NOT NULL,
  code_hash TEXT NOT NULL, author TEXT NOT NULL REFERENCES agents(id), at TEXT NOT NULL,
  UNIQUE (strategy_id, version));
CREATE TABLE IF NOT EXISTS backtest_runs (
  id INTEGER PRIMARY KEY, strategy_id INTEGER NOT NULL REFERENCES strategies(id), version INTEGER NOT NULL,
  agent_id TEXT NOT NULL REFERENCES agents(id), official INTEGER NOT NULL,
  inputs_hash TEXT NOT NULL, results_hash TEXT NOT NULL, at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS state_transitions (
  id INTEGER PRIMARY KEY, strategy_id INTEGER NOT NULL REFERENCES strategies(id), from_state TEXT NOT NULL,
  to_state TEXT NOT NULL, version INTEGER NOT NULL, at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS approvals (
  transition_id INTEGER NOT NULL REFERENCES state_transitions(id), agent_id TEXT NOT NULL REFERENCES agents(id),
  role TEXT NOT NULL, human INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS halts (
  id INTEGER PRIMARY KEY, strategy_id INTEGER NOT NULL REFERENCES strategies(id), previous_state TEXT NOT NULL,
  reason TEXT NOT NULL, agent_id TEXT NOT NULL REFERENCES agents(id), at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS risk_limits (
  strategy_id INTEGER NOT NULL REFERENCES strategies(id), name TEXT NOT NULL, value TEXT NOT NULL,
  version INTEGER NOT NULL, approver TEXT NOT NULL REFERENCES agents(id), at TEXT NOT NULL,
  UNIQUE (strategy_id, name, version));
CREATE TABLE IF NOT EXISTS paper_stats (
  strategy_id INTEGER NOT NULL REFERENCES strategies(id), version INTEGER NOT NULL, days INTEGER NOT NULL,
  trades INTEGER NOT NULL, agent_id TEXT NOT NULL REFERENCES agents(id), at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS budgets (
  agent_id TEXT NOT NULL REFERENCES agents(id), month TEXT NOT NULL, limit_usd TEXT NOT NULL,
  spent_usd TEXT NOT NULL, PRIMARY KEY (agent_id, month));
CREATE TABLE IF NOT EXISTS audit_log (
  seq INTEGER PRIMARY KEY, at TEXT NOT NULL, agent_id TEXT NOT NULL, action TEXT NOT NULL,
  detail TEXT NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL);
";

/// Every limit is a maximum, so a lower value is tighter.
pub const LIMIT_NAMES: [&str; 7] = [
    "max_position_usd",
    "max_account_pct",
    "max_daily_loss_usd",
    "max_drawdown_pct",
    "max_trades_per_day",
    "max_open_positions",
    "max_correlated_exposure_pct",
];

pub struct Store {
    pool: SqlitePool,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Names, ids and hashes are short and plain, so they can't carry anything else.
fn plain(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " :_-.".contains(c));
    if ok {
        Ok(())
    } else {
        Err(DeskError::Denied(format!(
            "{what} must be 1-100 plain characters"
        )))
    }
}

/// One signer of a lifecycle move: an agent id, plus the owner's proof when the Board signs.
pub struct Signer {
    pub agent_id: String,
    pub proof: Option<HumanProof>,
}

impl Store {
    /// Opens (creating if needed) the database at `url`, e.g. `sqlite://data/desk/desk.db` or
    /// `sqlite::memory:`.
    pub async fn open(url: &str) -> Result<Store> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true)
            // so INSERT OR REPLACE fires the delete guards too
            .pragma("recursive_triggers", "ON");
        // one connection, never recycled: writes are serialized and an in-memory database stays one
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        for t in APPEND_ONLY {
            sqlx::raw_sql(&format!(
                "CREATE TRIGGER IF NOT EXISTS {t}_no_update BEFORE UPDATE ON {t}
                   BEGIN SELECT RAISE(ABORT, '{t} is append-only'); END;
                 CREATE TRIGGER IF NOT EXISTS {t}_no_delete BEFORE DELETE ON {t}
                   BEGIN SELECT RAISE(ABORT, '{t} is append-only'); END;"
            ))
            .execute(&pool)
            .await?;
        }
        Ok(Store { pool })
    }

    /// The owner adds an agent. Agents can't add agents (fixed org chart). The very first agent
    /// must be the owner (Board) itself.
    pub async fn add_agent(
        &self,
        by: &str,
        id: &str,
        role: Role,
        proof: Option<HumanProof>,
    ) -> Result<()> {
        plain("agent id", id)?;
        if proof.is_none() {
            return Err(DeskError::Denied(
                "changing the org chart needs the owner's Touch ID".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents")
            .fetch_one(&mut *tx)
            .await?;
        if count == 0 {
            if role != Role::Board || id != by {
                return Err(DeskError::Denied(
                    "the first agent must be the owner (Board)".into(),
                ));
            }
        } else if role_in(&mut tx, by).await? != Role::Board {
            return Err(DeskError::Denied(
                "only the owner changes the org chart".into(),
            ));
        } else if role == Role::Board {
            return Err(DeskError::Denied("there is one owner".into()));
        }
        sqlx::query("INSERT INTO agents (id, role, created) VALUES (?, ?, ?)")
            .bind(id)
            .bind(role.as_str())
            .bind(now())
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            by,
            "add_agent",
            json!({"id": id, "role": role.as_str()}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// An agent's role, from the store (never from the agent's own claim).
    pub async fn role(&self, agent_id: &str) -> Result<Role> {
        let mut tx = self.pool.begin().await?;
        role_in(&mut tx, agent_id).await
    }

    /// Permission, and a budget that isn't exhausted. Halting and raising a ticket always work.
    pub async fn authorize(&self, agent_id: &str, action: Action) -> Result<Role> {
        let role = self.role(agent_id).await?;
        if !permissions::allowed(role, action) {
            return Err(DeskError::Denied(format!(
                "{} may not {action:?}",
                role.as_str()
            )));
        }
        if !matches!(action, Action::Halt | Action::CreateTicket) {
            let row: Option<(String, String)> = sqlx::query_as(
                "SELECT limit_usd, spent_usd FROM budgets WHERE agent_id = ? AND month = ?",
            )
            .bind(agent_id)
            .bind(month())
            .fetch_optional(&self.pool)
            .await?;
            if let Some((limit, spent)) = row
                && dec(&spent)? >= dec(&limit)?
            {
                return Err(DeskError::Budget(format!(
                    "{agent_id} has used its ${limit} this month"
                )));
            }
        }
        Ok(role)
    }

    /// The owner sets an agent's monthly budget.
    pub async fn set_budget(
        &self,
        by: &str,
        agent_id: &str,
        limit: Decimal,
        proof: HumanProof,
    ) -> Result<()> {
        let _ = proof; // consumed: one proof, one action
        if limit < Decimal::ZERO {
            return Err(DeskError::Denied("a budget can't be negative".into()));
        }
        let mut tx = self.pool.begin().await?;
        if role_in(&mut tx, by).await? != Role::Board {
            return Err(DeskError::Denied("only the owner sets budgets".into()));
        }
        role_in(&mut tx, agent_id).await?;
        sqlx::query(
            "INSERT INTO budgets (agent_id, month, limit_usd, spent_usd) VALUES (?, ?, ?, '0')
             ON CONFLICT (agent_id, month) DO UPDATE SET limit_usd = excluded.limit_usd",
        )
        .bind(agent_id)
        .bind(month())
        .bind(limit.to_string())
        .execute(&mut *tx)
        .await?;
        append(
            &mut tx,
            by,
            "set_budget",
            json!({"agent": agent_id, "limit": limit.to_string()}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reserves `cost` before a model call, atomically: refused, and nothing charged, if it would
    /// pass the month's limit. An agent with no budget can't spend.
    pub async fn spend(&self, agent_id: &str, cost: Decimal) -> Result<()> {
        if cost < Decimal::ZERO {
            return Err(DeskError::Denied("a cost can't be negative".into()));
        }
        let m = month();
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT limit_usd, spent_usd FROM budgets WHERE agent_id = ? AND month = ?",
        )
        .bind(agent_id)
        .bind(&m)
        .fetch_optional(&mut *tx)
        .await?;
        let (limit, spent) =
            row.ok_or_else(|| DeskError::Budget(format!("{agent_id} has no budget this month")))?;
        let (limit, spent) = (dec(&limit)?, dec(&spent)?);
        if spent + cost > limit {
            return Err(DeskError::Budget(format!(
                "{agent_id} has spent ${spent} of ${limit}"
            )));
        }
        sqlx::query("UPDATE budgets SET spent_usd = ? WHERE agent_id = ? AND month = ?")
            .bind((spent + cost).to_string())
            .bind(agent_id)
            .bind(&m)
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            agent_id,
            "spend",
            json!({"usd": cost.to_string(), "month": m}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A Researcher's hypothesis becomes a strategy in IDEA.
    pub async fn create_strategy(&self, researcher: &str, name: &str) -> Result<i64> {
        plain("strategy name", name)?;
        self.authorize(researcher, Action::CreateHypothesis).await?;
        let mut tx = self.pool.begin().await?;
        let id = sqlx::query(
            "INSERT INTO strategies (name, state, version, hypothesis_author) VALUES (?, 'idea', 0, ?)",
        )
        .bind(name)
        .bind(researcher)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        append(
            &mut tx,
            researcher,
            "create_strategy",
            json!({"id": id, "name": name}),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// The Strategist writes the pre-registered success criteria: only before the spec is frozen
    /// and before any backtest. The criteria's hash goes into the audit chain.
    pub async fn set_criteria(
        &self,
        strategist: &str,
        strategy: i64,
        criteria: &str,
    ) -> Result<()> {
        self.authorize(strategist, Action::EditSpec).await?;
        let mut tx = self.pool.begin().await?;
        let state = state_in(&mut tx, strategy).await?;
        if !matches!(state, State::Idea | State::HypothesisApproved) {
            return Err(DeskError::Denied(format!(
                "criteria are frozen at {}",
                state.as_str()
            )));
        }
        let runs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM backtest_runs WHERE strategy_id = ?")
                .bind(strategy)
                .fetch_one(&mut *tx)
                .await?;
        if runs > 0 {
            return Err(DeskError::Denied(
                "success criteria are frozen once a backtest is recorded".into(),
            ));
        }
        let hash = sha256(criteria);
        sqlx::query(
            "UPDATE strategies SET criteria = ?, criteria_hash = ?, spec_author = ? WHERE id = ?",
        )
        .bind(criteria)
        .bind(&hash)
        .bind(strategist)
        .bind(strategy)
        .execute(&mut *tx)
        .await?;
        append(
            &mut tx,
            strategist,
            "set_criteria",
            json!({"strategy": strategy, "criteria_sha256": hash}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The Developer submits code (by content hash) for review, only between SPEC_FROZEN and
    /// IMPLEMENTED: changing code after validation means failing back first, so every later gate
    /// is signed again for the new version. Fills a version left empty by a failed gate, or else
    /// starts the next one.
    pub async fn submit_version(
        &self,
        developer: &str,
        strategy: i64,
        code_hash: &str,
    ) -> Result<i64> {
        plain("code hash", code_hash)?;
        self.authorize(developer, Action::SubmitStrategyVersion)
            .await?;
        let mut tx = self.pool.begin().await?;
        let state = state_in(&mut tx, strategy).await?;
        if !matches!(state, State::SpecFrozen | State::Implemented) {
            return Err(DeskError::Denied(format!(
                "code can't change at {}; fail the strategy back to the spec or implementation first",
                state.as_str()
            )));
        }
        let current = version_in(&mut tx, strategy).await?;
        let has_code: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM strategy_versions WHERE strategy_id = ? AND version = ?",
        )
        .bind(strategy)
        .bind(current)
        .fetch_optional(&mut *tx)
        .await?;
        let version = if has_code.is_some() || current == 0 {
            current + 1
        } else {
            current
        };
        sqlx::query("UPDATE strategies SET version = ? WHERE id = ?")
            .bind(version)
            .bind(strategy)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO strategy_versions (strategy_id, version, code_hash, author, at) VALUES (?, ?, ?, ?, ?)")
            .bind(strategy)
            .bind(version)
            .bind(code_hash)
            .bind(developer)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            developer,
            "submit_version",
            json!({"strategy": strategy, "version": version, "code_hash": code_hash}),
        )
        .await?;
        tx.commit().await?;
        Ok(version)
    }

    /// Records a backtest of the current version by its input and result hashes. Developers and
    /// Researchers run development backtests; only the Validator runs official ones.
    pub async fn record_backtest(
        &self,
        agent_id: &str,
        strategy: i64,
        official: bool,
        inputs_hash: &str,
        results_hash: &str,
    ) -> Result<()> {
        plain("inputs hash", inputs_hash)?;
        plain("results hash", results_hash)?;
        let action = if official {
            Action::RunOfficialBacktest
        } else {
            Action::RunDevBacktest
        };
        self.authorize(agent_id, action).await?;
        let mut tx = self.pool.begin().await?;
        let version = version_in(&mut tx, strategy).await?;
        sqlx::query(
            "INSERT INTO backtest_runs (strategy_id, version, agent_id, official, inputs_hash, results_hash, at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(strategy)
        .bind(version)
        .bind(agent_id)
        .bind(official)
        .bind(inputs_hash)
        .bind(results_hash)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        append(
            &mut tx,
            agent_id,
            "record_backtest",
            json!({"strategy": strategy, "version": version, "official": official, "inputs": inputs_hash, "results": results_hash}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ops records the paper results the PAPER_REVIEWED gate reads (never the caller's numbers).
    pub async fn record_paper(
        &self,
        agent_id: &str,
        strategy: i64,
        days: u32,
        trades: u32,
    ) -> Result<()> {
        if self.role(agent_id).await? != Role::Ops {
            return Err(DeskError::Denied("only Ops records paper results".into()));
        }
        let mut tx = self.pool.begin().await?;
        let version = version_in(&mut tx, strategy).await?;
        sqlx::query("INSERT INTO paper_stats (strategy_id, version, days, trades, agent_id, at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(strategy)
            .bind(version)
            .bind(days)
            .bind(trades)
            .bind(agent_id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            agent_id,
            "record_paper",
            json!({"strategy": strategy, "version": version, "days": days, "trades": trades}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn state(&self, strategy: i64) -> Result<State> {
        let mut tx = self.pool.begin().await?;
        state_in(&mut tx, strategy).await
    }

    pub async fn version(&self, strategy: i64) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        version_in(&mut tx, strategy).await
    }

    /// Moves a strategy, with every required signature (`lifecycle::check`) and the evidence each
    /// gate needs, read from the store. Each proof is used up by this one move.
    pub async fn transition(&self, strategy: i64, to: State, signers: Vec<Signer>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let mut sigs = Vec::new();
        for s in signers {
            let role = role_in(&mut tx, &s.agent_id).await?;
            // a proof only counts for the Board's own identity
            sigs.push(Signature {
                human: role == Role::Board && s.proof.is_some(),
                agent_id: s.agent_id,
                role,
            });
        }
        let (state, version, hyp_author, spec_author, criteria, current_halt): (
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT state, version, hypothesis_author, spec_author, criteria, current_halt FROM strategies WHERE id = ?",
        )
        .bind(strategy)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| DeskError::NotFound(format!("strategy {strategy}")))?;
        let from = State::parse(&state)
            .ok_or_else(|| DeskError::Corrupt(format!("stored state {state:?}")))?;
        let halted = match current_halt {
            Some(h) if from == State::Halted => {
                let (prev, reason): (String, String) = sqlx::query_as(
                    "SELECT previous_state, reason FROM halts WHERE id = ? AND strategy_id = ?",
                )
                .bind(h)
                .bind(strategy)
                .fetch_one(&mut *tx)
                .await?;
                Some((
                    State::parse(&prev)
                        .ok_or_else(|| DeskError::Corrupt(format!("halt state {prev:?}")))?,
                    HaltReason::parse(&reason)
                        .ok_or_else(|| DeskError::Corrupt(format!("halt reason {reason:?}")))?,
                ))
            }
            _ => None,
        };
        let code_author: Option<String> = sqlx::query_scalar(
            "SELECT author FROM strategy_versions WHERE strategy_id = ? AND version = ?",
        )
        .bind(strategy)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;
        // the artifact being judged: the hypothesis, then the spec, then the code
        let author_id = match to {
            State::HypothesisApproved => hyp_author.clone(),
            State::SpecFrozen => spec_author.clone().unwrap_or_else(|| hyp_author.clone()),
            _ => code_author
                .clone()
                .or(spec_author.clone())
                .unwrap_or_else(|| hyp_author.clone()),
        };
        let author_role = role_in(&mut tx, &author_id).await?;
        // the evidence each forward gate needs
        let missing = match to {
            State::SpecFrozen if criteria.is_none() => Some("pre-registered criteria"),
            State::Implemented if code_author.is_none() => Some("submitted code for this version"),
            State::Validated if !official_run(&mut tx, strategy, version).await? => {
                Some("an official backtest of this version")
            }
            State::LimitsSet => {
                let n: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM risk_limits WHERE strategy_id = ?")
                        .bind(strategy)
                        .fetch_one(&mut *tx)
                        .await?;
                (n == 0).then_some("approved risk limits")
            }
            _ => None,
        };
        let paper: Option<PaperEvidence> = sqlx::query_as::<_, (i64, i64)>(
            "SELECT days, trades FROM paper_stats WHERE strategy_id = ? AND version = ? ORDER BY rowid DESC LIMIT 1",
        )
        .bind(strategy)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?
        .map(|(d, t)| PaperEvidence { days: d.max(0) as u32, trades: t.max(0) as u32 });
        // missing evidence is the real reason a forward gate can't pass, so it is reported first
        if let Some(what) = missing
            && lifecycle::rule(from, to, halted).is_some_and(|r| r.kind == Kind::Forward)
        {
            return Err(DeskError::Denied(format!("{} needs {what}", to.as_str())));
        }
        let rule = lifecycle::check(from, to, halted, (&author_id, author_role), &sigs, paper)?;
        // a failed gate starts a new version, which the Developer must fill before it moves on
        let new_version = if rule.kind == Kind::Back {
            version + 1
        } else {
            version
        };
        let halt = if rule.kind == Kind::Resume {
            None
        } else {
            current_halt.filter(|_| from == State::Halted)
        };
        sqlx::query("UPDATE strategies SET state = ?, version = ?, current_halt = ? WHERE id = ? AND state = ?")
            .bind(to.as_str())
            .bind(new_version)
            .bind(halt)
            .bind(strategy)
            .bind(from.as_str())
            .execute(&mut *tx)
            .await?;
        let t = sqlx::query(
            "INSERT INTO state_transitions (strategy_id, from_state, to_state, version, at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(strategy)
        .bind(from.as_str())
        .bind(to.as_str())
        .bind(new_version)
        .bind(now())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        for s in &sigs {
            sqlx::query(
                "INSERT INTO approvals (transition_id, agent_id, role, human) VALUES (?, ?, ?, ?)",
            )
            .bind(t)
            .bind(&s.agent_id)
            .bind(s.role.as_str())
            .bind(s.human)
            .execute(&mut *tx)
            .await?;
        }
        let signed: Vec<_> = sigs
            .iter()
            .map(|s| json!({"agent": s.agent_id, "role": s.role.as_str(), "human": s.human}))
            .collect();
        append(
            &mut tx,
            &sigs[0].agent_id,
            "transition",
            json!({"strategy": strategy, "from": from.as_str(), "to": to.as_str(), "version": new_version, "signers": signed}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Anyone may halt; only the second line that owns a risk or compliance halt may call one
    /// (`HaltReason::may_call`). The halt record decides who may resume, and to where.
    pub async fn halt(&self, agent_id: &str, strategy: i64, reason: HaltReason) -> Result<()> {
        let role = self.authorize(agent_id, Action::Halt).await?;
        if !reason.may_call(role) {
            return Err(DeskError::Denied(format!(
                "{} may not call a {} halt",
                role.as_str(),
                reason.as_str()
            )));
        }
        let mut tx = self.pool.begin().await?;
        let from = state_in(&mut tx, strategy).await?;
        if from == State::Halted || from == State::Retired {
            return Err(DeskError::Illegal(format!("already {}", from.as_str())));
        }
        let id = sqlx::query("INSERT INTO halts (strategy_id, previous_state, reason, agent_id, at) VALUES (?, ?, ?, ?, ?)")
            .bind(strategy)
            .bind(from.as_str())
            .bind(reason.as_str())
            .bind(agent_id)
            .bind(now())
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        sqlx::query(
            "UPDATE strategies SET state = 'halted', current_halt = ? WHERE id = ? AND state = ?",
        )
        .bind(id)
        .bind(strategy)
        .bind(from.as_str())
        .execute(&mut *tx)
        .await?;
        append(
            &mut tx,
            agent_id,
            "halt",
            json!({"strategy": strategy, "from": from.as_str(), "reason": reason.as_str(), "halt": id}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets a risk limit (all maximums: lower is tighter). The Risk Manager or the Board may
    /// tighten; only the Board, with the owner's proof, may set a first value or loosen one.
    pub async fn set_limit(
        &self,
        agent_id: &str,
        strategy: i64,
        name: &str,
        value: Decimal,
        proof: Option<HumanProof>,
    ) -> Result<()> {
        if !LIMIT_NAMES.contains(&name) {
            return Err(DeskError::Denied(format!("unknown limit {name:?}")));
        }
        if value < Decimal::ZERO {
            return Err(DeskError::Denied("a limit can't be negative".into()));
        }
        let mut tx = self.pool.begin().await?;
        let role = role_in(&mut tx, agent_id).await?;
        state_in(&mut tx, strategy).await?;
        let current: Option<(String, i64)> = sqlx::query_as(
            "SELECT value, version FROM risk_limits WHERE strategy_id = ? AND name = ? ORDER BY version DESC LIMIT 1",
        )
        .bind(strategy)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
        let (tighter, version) = match &current {
            Some((v, n)) => (value < dec(v)?, n + 1),
            None => (false, 1),
        };
        let action = if tighter {
            Action::TightenLimit
        } else {
            Action::LoosenLimit
        };
        if !permissions::allowed(role, action) {
            return Err(DeskError::Denied(format!(
                "{} may not {}",
                role.as_str(),
                if tighter {
                    "tighten limits"
                } else {
                    "set or loosen a limit"
                }
            )));
        }
        if action == Action::LoosenLimit && proof.is_none() {
            return Err(DeskError::Denied(
                "setting or loosening a limit needs the owner's Touch ID".into(),
            ));
        }
        sqlx::query("INSERT INTO risk_limits (strategy_id, name, value, version, approver, at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(strategy)
            .bind(name)
            .bind(value.to_string())
            .bind(version)
            .bind(agent_id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            agent_id,
            "set_limit",
            json!({"strategy": strategy, "name": name, "value": value.to_string(), "version": version, "human": proof.is_some()}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn audit_entries(&self) -> Result<Vec<Entry>> {
        let rows: Vec<(i64, String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT seq, at, agent_id, action, detail, prev_hash, hash FROM audit_log ORDER BY seq",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(entry).collect())
    }

    /// The daily check: the whole chain must hold, and a desk with agents can't have an empty log.
    pub async fn verify_audit(&self) -> Result<usize> {
        let e = self.audit_entries().await?;
        let agents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents")
            .fetch_one(&self.pool)
            .await?;
        if e.is_empty() && agents > 0 {
            return Err(DeskError::Tampered(0));
        }
        match audit::first_break(&e) {
            Some(seq) => Err(DeskError::Tampered(seq)),
            None => Ok(e.len()),
        }
    }

    #[cfg(test)]
    fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

fn entry(
    (seq, at, agent_id, action, detail, prev_hash, hash): (
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
    ),
) -> Entry {
    Entry {
        seq,
        at,
        agent_id,
        action,
        detail,
        prev_hash,
        hash,
    }
}

fn dec(s: &str) -> Result<Decimal> {
    Decimal::from_str(s).map_err(|e| DeskError::Corrupt(format!("stored amount {s:?}: {e}")))
}

async fn role_in(tx: &mut Transaction<'_, Sqlite>, agent_id: &str) -> Result<Role> {
    let r: Option<String> = sqlx::query_scalar("SELECT role FROM agents WHERE id = ?")
        .bind(agent_id)
        .fetch_optional(&mut **tx)
        .await?;
    let r = r.ok_or_else(|| DeskError::NotFound(format!("agent {agent_id}")))?;
    Role::parse(&r).ok_or_else(|| DeskError::Corrupt(format!("stored role {r:?}")))
}

async fn state_in(tx: &mut Transaction<'_, Sqlite>, strategy: i64) -> Result<State> {
    let s: Option<String> = sqlx::query_scalar("SELECT state FROM strategies WHERE id = ?")
        .bind(strategy)
        .fetch_optional(&mut **tx)
        .await?;
    let s = s.ok_or_else(|| DeskError::NotFound(format!("strategy {strategy}")))?;
    State::parse(&s).ok_or_else(|| DeskError::Corrupt(format!("stored state {s:?}")))
}

async fn version_in(tx: &mut Transaction<'_, Sqlite>, strategy: i64) -> Result<i64> {
    sqlx::query_scalar("SELECT version FROM strategies WHERE id = ?")
        .bind(strategy)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| DeskError::NotFound(format!("strategy {strategy}")))
}

async fn official_run(
    tx: &mut Transaction<'_, Sqlite>,
    strategy: i64,
    version: i64,
) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM backtest_runs WHERE strategy_id = ? AND version = ? AND official = 1",
    )
    .bind(strategy)
    .bind(version)
    .fetch_one(&mut **tx)
    .await?;
    Ok(n > 0)
}

/// Appends one audit entry, chained to the last, inside the caller's transaction.
async fn append(
    tx: &mut Transaction<'_, Sqlite>,
    agent_id: &str,
    action: &str,
    detail: serde_json::Value,
) -> Result<()> {
    let last: Option<(i64, String, String, String, String, String, String)> =
        sqlx::query_as("SELECT seq, at, agent_id, action, detail, prev_hash, hash FROM audit_log ORDER BY seq DESC LIMIT 1")
            .fetch_optional(&mut **tx)
            .await?;
    let e = audit::next(
        last.map(entry).as_ref(),
        &now(),
        agent_id,
        action,
        &detail.to_string(),
    );
    sqlx::query("INSERT INTO audit_log (seq, at, agent_id, action, detail, prev_hash, hash) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(e.seq)
        .bind(&e.at)
        .bind(&e.agent_id)
        .bind(&e.action)
        .bind(&e.detail)
        .bind(&e.prev_hash)
        .bind(&e.hash)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof() -> HumanProof {
        HumanProof::for_test()
    }

    async fn desk() -> Store {
        let s = Store::open("sqlite::memory:").await.unwrap();
        s.add_agent("board", "board", Role::Board, Some(proof()))
            .await
            .unwrap();
        for (id, role) in [
            ("res-1", Role::Researcher),
            ("strat-1", Role::Strategist),
            ("dev-1", Role::Developer),
            ("dev-2", Role::Developer),
            ("val-1", Role::Validator),
            ("stress-1", Role::StressLead),
            ("risk-1", Role::RiskManager),
            ("comp-1", Role::Compliance),
            ("ops-1", Role::Ops),
            ("po-1", Role::ProductOwner),
        ] {
            s.add_agent("board", id, role, Some(proof())).await.unwrap();
        }
        s
    }

    fn by(id: &str) -> Vec<Signer> {
        vec![Signer {
            agent_id: id.into(),
            proof: None,
        }]
    }

    fn owner() -> Vec<Signer> {
        vec![Signer {
            agent_id: "board".into(),
            proof: Some(proof()),
        }]
    }

    fn d(x: &str) -> Decimal {
        Decimal::from_str(x).unwrap()
    }

    fn denied<T>(r: Result<T>, text: &str) -> bool {
        matches!(r, Err(DeskError::Denied(ref m)) if m.contains(text))
    }

    /// A strategy taken through the front-office steps to IMPLEMENTED.
    async fn implemented(s: &Store) -> i64 {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!(
            "idea {}",
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let id = s.create_strategy("res-1", &name).await.unwrap();
        s.transition(id, State::HypothesisApproved, by("strat-1"))
            .await
            .unwrap();
        s.set_criteria("strat-1", id, "{\"min_ic\":0.02}")
            .await
            .unwrap();
        s.transition(id, State::SpecFrozen, by("po-1"))
            .await
            .unwrap();
        s.submit_version("dev-1", id, "sha256:abc").await.unwrap();
        s.transition(id, State::Implemented, by("strat-1"))
            .await
            .unwrap();
        id
    }

    /// The same strategy, on through PAPER_TRADING.
    async fn paper(s: &Store) -> i64 {
        let id = implemented(s).await;
        s.record_backtest("val-1", id, true, "in", "out")
            .await
            .unwrap();
        s.transition(id, State::Validated, by("val-1"))
            .await
            .unwrap();
        s.transition(id, State::StressTested, by("stress-1"))
            .await
            .unwrap();
        s.set_limit("board", id, "max_daily_loss_usd", d("500"), Some(proof()))
            .await
            .unwrap();
        s.transition(id, State::LimitsSet, owner()).await.unwrap();
        s.transition(id, State::ComplianceCleared, by("comp-1"))
            .await
            .unwrap();
        s.transition(id, State::PaperTrading, by("ops-1"))
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn an_agent_cant_approve_its_own_artifact() {
        let s = desk().await;
        let id = implemented(&s).await;
        s.record_backtest("val-1", id, true, "in", "out")
            .await
            .unwrap();
        // the code's author can't sign the gate that judges it, nor can a peer in the same role
        assert!(denied(
            s.transition(id, State::Validated, by("dev-1")).await,
            "maker-checker"
        ));
        assert!(denied(
            s.transition(id, State::Validated, by("dev-2")).await,
            "maker-checker"
        ));
        s.transition(id, State::Validated, by("val-1"))
            .await
            .unwrap();
        // a two-signer gate refuses the author among its signers
        let id2 = paper(&s).await;
        s.record_paper("ops-1", id2, 25, 40).await.unwrap();
        let with_author = vec![
            Signer {
                agent_id: "dev-1".into(),
                proof: None,
            },
            Signer {
                agent_id: "risk-1".into(),
                proof: None,
            },
        ];
        assert!(denied(
            s.transition(id2, State::PaperReviewed, with_author).await,
            "maker-checker"
        ));
    }

    #[tokio::test]
    async fn gates_need_their_evidence_and_nothing_skips() {
        let s = desk().await;
        let id = s.create_strategy("res-1", "no criteria").await.unwrap();
        s.transition(id, State::HypothesisApproved, by("strat-1"))
            .await
            .unwrap();
        assert!(denied(
            s.transition(id, State::SpecFrozen, by("po-1")).await,
            "criteria"
        ));
        s.set_criteria("strat-1", id, "{}").await.unwrap();
        s.transition(id, State::SpecFrozen, by("po-1"))
            .await
            .unwrap();
        assert!(denied(
            s.transition(id, State::Implemented, by("strat-1")).await,
            "submitted code"
        ));
        s.submit_version("dev-1", id, "sha256:one").await.unwrap();
        s.transition(id, State::Implemented, by("strat-1"))
            .await
            .unwrap();
        assert!(matches!(
            s.transition(id, State::StressTested, by("stress-1")).await,
            Err(DeskError::Illegal(_))
        ));
        assert!(denied(
            s.transition(id, State::Validated, by("val-1")).await,
            "official backtest"
        ));
        s.record_backtest("val-1", id, true, "in", "out")
            .await
            .unwrap();
        s.transition(id, State::Validated, by("val-1"))
            .await
            .unwrap();
        s.transition(id, State::StressTested, by("stress-1"))
            .await
            .unwrap();
        assert!(denied(
            s.transition(id, State::LimitsSet, owner()).await,
            "risk limits"
        ));
        // halting through a transition is refused: only `halt` records a reason
        assert!(matches!(
            s.transition(id, State::Halted, by("ops-1")).await,
            Err(DeskError::Illegal(_))
        ));
    }

    #[tokio::test]
    async fn code_cant_change_after_validation_without_failing_back() {
        let s = desk().await;
        let id = implemented(&s).await;
        assert_eq!(s.version(id).await.unwrap(), 1);
        s.record_backtest("val-1", id, true, "in", "out")
            .await
            .unwrap();
        s.transition(id, State::Validated, by("val-1"))
            .await
            .unwrap();
        assert!(denied(
            s.submit_version("dev-1", id, "sha256:swap").await,
            "fail the strategy back"
        ));
        // a failed gate starts version 2, which has no code and no official run yet
        s.transition(id, State::Implemented, by("stress-1"))
            .await
            .unwrap();
        assert_eq!(s.version(id).await.unwrap(), 2);
        assert!(denied(
            s.transition(id, State::Validated, by("val-1")).await,
            "official backtest"
        ));
        assert_eq!(
            s.submit_version("dev-1", id, "sha256:fixed").await.unwrap(),
            2
        );
        s.record_backtest("val-1", id, true, "in2", "out2")
            .await
            .unwrap();
        s.transition(id, State::Validated, by("val-1"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn live_needs_the_owner_and_store_side_paper_evidence() {
        let s = desk().await;
        let id = paper(&s).await;
        let two = || {
            vec![
                Signer {
                    agent_id: "po-1".into(),
                    proof: None,
                },
                Signer {
                    agent_id: "risk-1".into(),
                    proof: None,
                },
            ]
        };
        assert!(denied(
            s.transition(id, State::PaperReviewed, two()).await,
            "paper review"
        ));
        s.record_paper("ops-1", id, 12, 31).await.unwrap();
        assert!(denied(
            s.transition(id, State::PaperReviewed, two()).await,
            "paper review"
        ));
        assert!(
            s.record_paper("dev-1", id, 99, 99).await.is_err(),
            "only Ops records paper results"
        );
        s.record_paper("ops-1", id, 21, 33).await.unwrap();
        s.transition(id, State::PaperReviewed, two()).await.unwrap();
        for agent in ["ops-1", "risk-1", "po-1", "val-1", "board"] {
            assert!(
                s.transition(id, State::LiveApproved, by(agent))
                    .await
                    .is_err(),
                "{agent}"
            );
        }
        let fake = vec![Signer {
            agent_id: "ops-1".into(),
            proof: Some(proof()),
        }];
        assert!(
            s.transition(id, State::LiveApproved, fake).await.is_err(),
            "a proof only counts for the Board"
        );
        s.transition(id, State::LiveApproved, owner())
            .await
            .unwrap();
        assert_eq!(s.state(id).await.unwrap(), State::LiveApproved);
    }

    #[tokio::test]
    async fn criteria_are_frozen_after_the_first_backtest_and_at_the_spec() {
        let s = desk().await;
        let id = s.create_strategy("res-1", "idea").await.unwrap();
        s.set_criteria("strat-1", id, "{\"min_ic\":0.02}")
            .await
            .unwrap();
        assert!(
            s.set_criteria("dev-1", id, "{}").await.is_err(),
            "only the Strategist edits the spec"
        );
        s.record_backtest("dev-1", id, false, "in", "out")
            .await
            .unwrap();
        assert!(denied(
            s.set_criteria("strat-1", id, "{\"min_ic\":0.01}").await,
            "frozen once a backtest"
        ));
        assert!(
            s.record_backtest("dev-1", id, true, "in", "out")
                .await
                .is_err(),
            "official runs are the Validator's"
        );
        let id2 = implemented(&s).await;
        assert!(denied(
            s.set_criteria("strat-1", id2, "{}").await,
            "frozen at implemented"
        ));
        // the criteria's hash is in the audit chain
        let log = s.audit_entries().await.unwrap();
        assert!(
            log.iter()
                .any(|e| e.action == "set_criteria"
                    && e.detail.contains(&sha256("{\"min_ic\":0.02}")))
        );
    }

    #[tokio::test]
    async fn only_the_owner_loosens_limits_or_lifts_a_risk_halt() {
        let s = desk().await;
        let id = implemented(&s).await;
        assert!(denied(
            s.set_limit("risk-1", id, "max_daily_loss_usd", d("500"), None)
                .await,
            "set or loosen"
        ));
        assert!(denied(
            s.set_limit("board", id, "max_daily_loss_usd", d("500"), None)
                .await,
            "Touch ID"
        ));
        s.set_limit("board", id, "max_daily_loss_usd", d("500"), Some(proof()))
            .await
            .unwrap();
        s.set_limit("risk-1", id, "max_daily_loss_usd", d("300"), None)
            .await
            .unwrap();
        assert!(denied(
            s.set_limit("risk-1", id, "max_daily_loss_usd", d("400"), None)
                .await,
            "set or loosen"
        ));
        s.set_limit("board", id, "max_daily_loss_usd", d("400"), Some(proof()))
            .await
            .unwrap();
        assert!(
            s.set_limit("risk-1", id, "max_daily_loss_usd", d("-1"), None)
                .await
                .is_err()
        );
        assert!(
            s.set_limit("board", id, "max_nonsense", d("1"), Some(proof()))
                .await
                .is_err()
        );
        // a risk halt: only the Risk Manager (or owner) may call it, only the owner lifts it
        assert!(s.halt("ops-1", id, HaltReason::RiskBreach).await.is_err());
        s.halt("risk-1", id, HaltReason::RiskBreach).await.unwrap();
        assert!(
            s.halt("ops-1", id, HaltReason::Other).await.is_err(),
            "already halted"
        );
        assert!(
            s.transition(id, State::Implemented, by("ops-1"))
                .await
                .is_err()
        );
        assert!(
            s.transition(id, State::Validated, owner()).await.is_err(),
            "not where it was"
        );
        s.transition(id, State::Implemented, owner()).await.unwrap();
        // any role may halt for another reason, and Ops may lift that, back to where it was
        s.halt("res-1", id, HaltReason::Other).await.unwrap();
        s.transition(id, State::Implemented, by("ops-1"))
            .await
            .unwrap();
        // a resume clears the halt record, so it can't be reused
        assert!(
            s.transition(id, State::Implemented, by("ops-1"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_budget_exhausted_agent_is_blocked() {
        let s = desk().await;
        assert!(
            matches!(s.spend("val-1", d("0.10")).await, Err(DeskError::Budget(_))),
            "no budget, no spend"
        );
        s.set_budget("board", "val-1", d("1.00"), proof())
            .await
            .unwrap();
        s.spend("val-1", d("0.60")).await.unwrap();
        assert!(
            matches!(s.spend("val-1", d("0.50")).await, Err(DeskError::Budget(_))),
            "would pass the limit"
        );
        assert!(
            s.spend("val-1", d("-5")).await.is_err(),
            "no refunds through spend"
        );
        s.spend("val-1", d("0.40")).await.unwrap();
        // spent == limit: even a zero-cost action is refused, though halting still works
        assert!(matches!(
            s.authorize("val-1", Action::RunOfficialBacktest).await,
            Err(DeskError::Budget(_))
        ));
        s.authorize("val-1", Action::Halt).await.unwrap();
        assert!(
            s.set_budget("val-1", "val-1", d("100"), proof())
                .await
                .is_err(),
            "only the owner"
        );
        assert!(
            s.set_budget("board", "ghost", d("1"), proof())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_org_chart_is_fixed_and_tamper_is_detected() {
        let s = desk().await;
        assert!(
            s.add_agent("dev-1", "dev-3", Role::Developer, Some(proof()))
                .await
                .is_err(),
            "agents can't hire"
        );
        assert!(
            s.add_agent("board", "dev-3", Role::Developer, None)
                .await
                .is_err(),
            "the owner needs Touch ID"
        );
        assert!(
            s.add_agent("board", "board-2", Role::Board, Some(proof()))
                .await
                .is_err(),
            "one owner"
        );
        let id = implemented(&s).await;
        s.halt("ops-1", id, HaltReason::Operational).await.unwrap();
        assert!(s.verify_audit().await.unwrap() > 10);
        // history tables refuse edits, deletes and replace-in-place
        for sql in [
            "UPDATE audit_log SET detail = 'x' WHERE seq = 3",
            "DELETE FROM audit_log WHERE seq = 3",
            "UPDATE halts SET reason = 'other'",
            "DELETE FROM approvals",
            "INSERT OR REPLACE INTO audit_log (seq, at, agent_id, action, detail, prev_hash, hash) VALUES (3, 'a', 'b', 'c', 'd', 'e', 'f')",
        ] {
            assert!(sqlx::query(sql).execute(s.pool()).await.is_err(), "{sql}");
        }
        // someone with file access drops a trigger and edits a row: the chain shows it
        sqlx::raw_sql("DROP TRIGGER audit_log_no_update")
            .execute(s.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE audit_log SET detail = '{}' WHERE seq = 3")
            .execute(s.pool())
            .await
            .unwrap();
        assert!(matches!(
            s.verify_audit().await,
            Err(DeskError::Tampered(3))
        ));
    }
}
