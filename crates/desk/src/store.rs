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
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Sqlite, Transaction};
use std::str::FromStr;

/// Tables whose rows are history: never updated, never deleted.
const APPEND_ONLY: [&str; 12] = [
    "equity_peaks",
    "evidence",
    "signatures",
    "owner_key",
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
CREATE TABLE IF NOT EXISTS evidence (
  id INTEGER PRIMARY KEY, strategy_id INTEGER NOT NULL REFERENCES strategies(id), version INTEGER NOT NULL,
  agent_id TEXT NOT NULL REFERENCES agents(id), inputs_hash TEXT NOT NULL, results_hash TEXT NOT NULL,
  body TEXT NOT NULL, at TEXT NOT NULL);
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
CREATE TABLE IF NOT EXISTS owner_key (
  id INTEGER PRIMARY KEY CHECK (id = 1), public_key TEXT NOT NULL, at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS owner_requests (
  id TEXT PRIMARY KEY, payload TEXT NOT NULL, requested_by TEXT NOT NULL REFERENCES agents(id),
  expires TEXT NOT NULL, status TEXT NOT NULL, at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS agent_tokens (
  token_sha256 TEXT PRIMARY KEY, agent_id TEXT NOT NULL REFERENCES agents(id), created TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS signatures (
  strategy_id INTEGER NOT NULL REFERENCES strategies(id), version INTEGER NOT NULL, to_state TEXT NOT NULL,
  agent_id TEXT NOT NULL REFERENCES agents(id), at TEXT NOT NULL,
  UNIQUE (strategy_id, version, to_state, agent_id));
CREATE TABLE IF NOT EXISTS equity_peaks (
  id INTEGER PRIMARY KEY, account TEXT NOT NULL CHECK (account IN ('paper', 'live')),
  cents INTEGER NOT NULL CHECK (cents > 0), agent_id TEXT NOT NULL REFERENCES agents(id), at TEXT NOT NULL);
CREATE TRIGGER IF NOT EXISTS equity_peaks_rise BEFORE INSERT ON equity_peaks
  WHEN NEW.cents <= (SELECT COALESCE(MAX(cents), 0) FROM equity_peaks WHERE account = NEW.account)
  BEGIN SELECT RAISE(ABORT, 'equity_peaks only rises'); END;
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
    pub(crate) pool: SqlitePool,
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

/// How many owner requests one agent may have waiting at once.
const MAX_OPEN_REQUESTS: i64 = 5;

/// `n` random bytes as hex, from the system's randomness.
pub(crate) fn random_hex(n: usize) -> Result<String> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| DeskError::Internal(format!("no randomness: {e}")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Names, ids and hashes are short and plain, so they can't carry anything else.
pub(crate) fn plain(what: &str, s: &str) -> Result<()> {
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

    /// The owner adds an agent (through `approve_owner_action`). Agents can't add agents (fixed
    /// org chart); the Board itself is created only by `init_owner`.
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
        if role == Role::Board {
            return Err(DeskError::Denied(
                "there is one owner, registered by `desk init`".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        if role_in(&mut tx, by).await? != Role::Board {
            return Err(DeskError::Denied(
                "only the owner changes the org chart".into(),
            ));
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

    /// Creates the Board agent `board` with the owner's public key, on an empty desk only. The
    /// caller (`init_owner`) has already checked the key signs with Touch ID.
    pub(crate) async fn bootstrap_owner(&self, public_hex: &str) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents")
            .fetch_one(&mut *tx)
            .await?;
        if count > 0 {
            return Err(DeskError::Denied("the desk already has an owner".into()));
        }
        sqlx::query("INSERT INTO agents (id, role, created) VALUES ('board', 'board', ?)")
            .bind(now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO owner_key (id, public_key, at) VALUES (1, ?, ?)")
            .bind(public_hex)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            "board",
            "init_owner",
            json!({"public_key": public_hex}),
        )
        .await?;
        let token = issue_token_in(&mut tx, "board").await?;
        tx.commit().await?;
        Ok(token)
    }

    pub(crate) async fn insert_owner_request(
        &self,
        by: &str,
        id: &str,
        payload: &str,
        expires: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let open: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM owner_requests WHERE requested_by = ? AND status = 'pending' AND expires > ?",
        )
        .bind(by)
        .bind(now())
        .fetch_one(&mut *tx)
        .await?;
        if open >= MAX_OPEN_REQUESTS {
            return Err(DeskError::Denied(format!(
                "{by} already has {MAX_OPEN_REQUESTS} requests waiting for the owner"
            )));
        }
        sqlx::query(
            "INSERT INTO owner_requests (id, payload, requested_by, expires, status, at) VALUES (?, ?, ?, ?, 'pending', ?)",
        )
        .bind(id)
        .bind(payload)
        .bind(by)
        .bind(expires)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        append(
            &mut tx,
            by,
            "request_owner",
            json!({"request": id, "payload": payload}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Open requests: (id, payload, requested by, expires).
    pub(crate) async fn open_owner_requests(
        &self,
        now: &str,
    ) -> Result<Vec<(String, String, String, String)>> {
        Ok(sqlx::query_as(
            "SELECT id, payload, requested_by, expires FROM owner_requests WHERE status = 'pending' AND expires > ? ORDER BY at",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Closes a pending request if `check(payload, expires, owner key)` passes, in one transaction,
    /// so a request is approved at most once; returns its payload.
    pub(crate) async fn redeem_owner_request(
        &self,
        id: &str,
        check: impl FnOnce(&str, &str, &str) -> Result<()>,
    ) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT payload, expires FROM owner_requests WHERE id = ? AND status = 'pending'",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let (payload, expires) = row.ok_or_else(|| {
            DeskError::Denied("no open request with that id (used, expired or unknown)".into())
        })?;
        let public: String = sqlx::query_scalar("SELECT public_key FROM owner_key WHERE id = 1")
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| DeskError::Denied("no owner registered: run `desk init`".into()))?;
        check(&payload, &expires, &public)?;
        let n = sqlx::query(
            "UPDATE owner_requests SET status = 'approved' WHERE id = ? AND status = 'pending'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n != 1 {
            return Err(DeskError::Denied("this request was just used".into()));
        }
        append(&mut tx, "board", "approve_owner", json!({"request": id})).await?;
        tx.commit().await?;
        Ok(payload)
    }

    /// Puts an approved request back to pending when its action failed, so the owner's signature
    /// isn't spent on nothing (each action is one transaction, so nothing of it was applied).
    pub(crate) async fn reopen_owner_request(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE owner_requests SET status = 'pending' WHERE id = ? AND status = 'approved'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        append(&mut tx, "board", "reopen_owner", json!({"request": id})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A new bearer token for an agent, shown once; only its SHA-256 is kept, and it replaces the
    /// agent's earlier tokens (so issuing again is also how a leaked token is revoked). Issued
    /// through an owner action, or to the Board at `init_owner`.
    pub(crate) async fn issue_token(&self, agent_id: &str) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let token = issue_token_in(&mut tx, agent_id).await?;
        tx.commit().await?;
        Ok(token)
    }

    /// The agent a bearer token belongs to.
    pub async fn agent_for_token(&self, token: &str) -> Result<String> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT agent_id FROM agent_tokens WHERE token_sha256 = ?")
                .bind(sha256(token))
                .fetch_optional(&self.pool)
                .await?;
        id.ok_or_else(|| DeskError::Denied("unknown token".into()))
    }

    /// An agent signs a lifecycle step for the strategy's current version. The step happens once
    /// the signatures collected so far cover its required roles; until then the signature waits.
    /// Returns true if the strategy moved.
    pub async fn sign(&self, agent_id: &str, strategy: i64, to: State) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let role = role_in(&mut tx, agent_id).await?;
        if role == Role::Board {
            return Err(DeskError::Denied(
                "the owner signs through an owner request (Touch ID)".into(),
            ));
        }
        // the engine never signs by hand; its one fail back is the automatic one (S46)
        if role == Role::Engine {
            return Err(DeskError::Denied("the engine doesn't sign".into()));
        }
        if role == Role::Console {
            return Err(DeskError::Denied("the console only reads".into()));
        }
        // one transaction: the version read here is the one every collected signature is for
        let version: i64 = sqlx::query_scalar("SELECT version FROM strategies WHERE id = ?")
            .bind(strategy)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| DeskError::NotFound(format!("strategy {strategy}")))?;
        let mut signers: Vec<String> = sqlx::query_scalar(
            "SELECT agent_id FROM signatures WHERE strategy_id = ? AND version = ? AND to_state = ? ORDER BY at",
        )
        .bind(strategy)
        .bind(version)
        .bind(to.as_str())
        .fetch_all(&mut *tx)
        .await?;
        if !signers.iter().any(|a| a == agent_id) {
            signers.push(agent_id.into());
        }
        let all = signers
            .into_iter()
            .map(|agent_id| Signer {
                agent_id,
                proof: None,
            })
            .collect();
        // the step is tried first, so a signature the gate would refuse is never kept
        let needs = match transition_in(&mut tx, strategy, to, all).await {
            Ok(()) => {
                tx.commit().await?;
                return Ok(true);
            }
            Err(DeskError::Unsigned(needs)) => needs,
            Err(e) => return Err(e),
        };
        if !needs.contains(&role) {
            return Err(DeskError::Denied(format!(
                "{} doesn't sign {}",
                role.as_str(),
                to.as_str()
            )));
        }
        sqlx::query(
            "INSERT OR IGNORE INTO signatures (strategy_id, version, to_state, agent_id, at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(strategy)
        .bind(version)
        .bind(to.as_str())
        .bind(agent_id)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        append(
            &mut tx,
            agent_id,
            "sign",
            json!({"strategy": strategy, "version": version, "to": to.as_str()}),
        )
        .await?;
        tx.commit().await?;
        Ok(false)
    }

    /// A strategy's risk limits, each at its latest version: (name, value).
    pub async fn limits(&self, strategy: i64) -> Result<Vec<(String, String)>> {
        self.state(strategy).await?;
        Ok(sqlx::query_as(
            "SELECT name, value FROM risk_limits r WHERE strategy_id = ?
               AND version = (SELECT MAX(version) FROM risk_limits WHERE strategy_id = r.strategy_id AND name = r.name)
             ORDER BY name",
        )
        .bind(strategy)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every strategy: (id, name, state, version).
    pub async fn strategies(&self) -> Result<Vec<(i64, String, String, i64)>> {
        Ok(
            sqlx::query_as("SELECT id, name, state, version FROM strategies ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
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

    /// This month's (limit, spent), if the agent has a budget.
    pub async fn budget(&self, agent_id: &str) -> Result<Option<(Decimal, Decimal)>> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT limit_usd, spent_usd FROM budgets WHERE agent_id = ? AND month = ?",
        )
        .bind(agent_id)
        .bind(month())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(l, s)| Ok((dec(&l)?, dec(&s)?))).transpose()
    }

    /// Every agent's (limit, spent) this month, by agent id (S47).
    pub async fn budgets(&self, agent_id: &str) -> Result<Vec<(String, Decimal, Decimal)>> {
        self.authorize(agent_id, Action::ReadBudgets).await?;
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT agent_id, limit_usd, spent_usd FROM budgets WHERE month = ? ORDER BY agent_id",
        )
        .bind(month())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(a, l, s)| Ok((a, dec(&l)?, dec(&s)?)))
            .collect()
    }

    /// Records what a model call actually cost, after it: always recorded, even past the limit,
    /// so the books are exact. The runner checks the room before each call (`budget`), and
    /// `authorize` blocks an agent once it's over. An agent with no budget can't be charged.
    pub async fn charge(&self, agent_id: &str, cost: Decimal) -> Result<()> {
        if cost < Decimal::ZERO {
            return Err(DeskError::Denied("a cost can't be negative".into()));
        }
        let m = month();
        let mut tx = self.pool.begin().await?;
        let spent: Option<(String,)> =
            sqlx::query_as("SELECT spent_usd FROM budgets WHERE agent_id = ? AND month = ?")
                .bind(agent_id)
                .bind(&m)
                .fetch_optional(&mut *tx)
                .await?;
        let (spent,) = spent
            .ok_or_else(|| DeskError::Budget(format!("{agent_id} has no budget this month")))?;
        sqlx::query("UPDATE budgets SET spent_usd = ? WHERE agent_id = ? AND month = ?")
            .bind(
                dec(&spent)?
                    .checked_add(cost)
                    .ok_or_else(|| DeskError::Denied("that charge overflows".into()))?
                    .to_string(),
            )
            .bind(agent_id)
            .bind(&m)
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            agent_id,
            "charge",
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
        // criteria are read by the signers' models; keep them short
        if criteria.len() > crate::evidence::MAX_CRITERIA {
            return Err(DeskError::Denied(format!(
                "criteria are over {} characters",
                crate::evidence::MAX_CRITERIA
            )));
        }
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

    /// Records the engine's code-computed evidence for the strategy's current version (S46): the
    /// desk hashes the body itself, records it as an official backtest (so a non-reproducing one
    /// fails back exactly as `record_backtest` does), and keeps the body. Returns whether it
    /// reproduces. Only `Role::Engine` may post.
    pub async fn post_evidence(
        &self,
        agent_id: &str,
        strategy: i64,
        inputs_hash: &str,
        body: &Value,
    ) -> Result<bool> {
        plain("inputs hash", inputs_hash)?;
        self.authorize(agent_id, Action::PostEvidence).await?;
        crate::evidence::validate(body).map_err(DeskError::Denied)?;
        let results_hash = crate::evidence::hash(body);
        // one transaction: the run and its body land together, on the version the run was for,
        // even when a non-reproducing run fails the strategy back to a new version
        let mut tx = self.pool.begin().await?;
        let (reproduces, version) = record_backtest_in(
            &mut tx,
            agent_id,
            strategy,
            true,
            inputs_hash,
            &results_hash,
        )
        .await?;
        sqlx::query(
            "INSERT INTO evidence (strategy_id, version, agent_id, inputs_hash, results_hash, body, at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(strategy)
        .bind(version)
        .bind(agent_id)
        .bind(inputs_hash)
        .bind(&results_hash)
        .bind(crate::evidence::canonical(body))
        .bind(now())
        .execute(&mut *tx)
        .await?;
        append(
            &mut tx,
            agent_id,
            "post_evidence",
            json!({"strategy": strategy, "version": version, "inputs": inputs_hash, "results": results_hash, "reproduces": reproduces}),
        )
        .await?;
        tx.commit().await?;
        Ok(reproduces)
    }

    /// An account's drawdown peak in cents and when it was set (R13 step 3d), or None before the
    /// first. `account` is "paper" or "live".
    pub async fn peak(&self, agent_id: &str, account: &str) -> Result<Option<(i64, String)>> {
        self.authorize(agent_id, Action::ReadPeak).await?;
        peak_account(account)?;
        Ok(sqlx::query_as(
            "SELECT cents, at FROM equity_peaks WHERE account = ? ORDER BY cents DESC LIMIT 1",
        )
        .bind(account)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Records `cents` as the account's peak if it beats the current one, and returns the peak
    /// after. The table only rises (a trigger refuses anything else) and is append-only, so no
    /// call can lower or reset it. The engine only, from code.
    pub async fn raise_peak(&self, agent_id: &str, account: &str, cents: i64) -> Result<i64> {
        self.authorize(agent_id, Action::RecordPeak).await?;
        peak_account(account)?;
        if cents <= 0 {
            return Err(DeskError::Denied(format!(
                "peak {cents} cents isn't positive"
            )));
        }
        let mut tx = self.pool.begin().await?;
        let (best,): (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(cents), 0) FROM equity_peaks WHERE account = ?")
                .bind(account)
                .fetch_one(&mut *tx)
                .await?;
        if cents <= best {
            return Ok(best);
        }
        sqlx::query("INSERT INTO equity_peaks (account, cents, agent_id, at) VALUES (?, ?, ?, ?)")
            .bind(account)
            .bind(cents)
            .bind(agent_id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            agent_id,
            "raise_peak",
            json!({"account": account, "cents": cents, "was": best}),
        )
        .await?;
        tx.commit().await?;
        Ok(cents)
    }

    /// What a signer needs for a strategy: its state, version and criteria, the latest evidence
    /// for the current version, each criterion checked against it in code, and the signatures
    /// already on the next step. Only roles with `ReadEvidence`.
    pub async fn evidence(&self, agent_id: &str, strategy: i64) -> Result<Value> {
        self.authorize(agent_id, Action::ReadEvidence).await?;
        let mut tx = self.pool.begin().await?;
        let (name, state, version, criteria): (String, String, i64, Option<String>) =
            sqlx::query_as("SELECT name, state, version, criteria FROM strategies WHERE id = ?")
                .bind(strategy)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| DeskError::NotFound(format!("strategy {strategy}")))?;
        let latest: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT inputs_hash, results_hash, body, at FROM evidence
             WHERE strategy_id = ? AND version = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(strategy)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;
        let signed: Vec<(String, String)> = sqlx::query_as(
            "SELECT to_state, agent_id FROM signatures WHERE strategy_id = ? AND version = ? ORDER BY at",
        )
        .bind(strategy)
        .bind(version)
        .fetch_all(&mut *tx)
        .await?;
        let evidence = match latest {
            None => None,
            Some((inputs, results, body, at)) => {
                let body: Value = serde_json::from_str(&body)
                    .map_err(|e| DeskError::Corrupt(format!("stored evidence: {e}")))?;
                let outcomes = criteria.as_deref().and_then(|c| {
                    body.get("metrics")
                        .and_then(Value::as_object)
                        .and_then(|m| crate::evidence::evaluate(c, m))
                });
                // only fields the engine writes reach a signer: no free text from anyone else
                let shown: serde_json::Map<String, Value> = body
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(k, _)| crate::evidence::SHOWN.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(json!({
                    "inputs_hash": inputs,
                    "results_hash": results,
                    "recorded": at,
                    "checkable": outcomes.is_some(),
                    "criteria_checks": outcomes,
                    // false, not null, when the criteria can't be checked: never a reason to sign
                    "all_criteria_pass": outcomes.as_deref().is_some_and(crate::evidence::all_pass),
                    "body": shown,
                }))
            }
        };
        Ok(json!({
            "strategy": strategy,
            "name": name,
            "state": state,
            "version": version,
            "criteria": criteria,
            "evidence": evidence,
            "signatures_this_version": signed.into_iter().map(|(to, by)| json!({"to": to, "by": by})).collect::<Vec<_>>(),
        }))
    }

    /// Records a backtest of the current version by its input and result hashes. Developers and
    /// Researchers run development backtests; only the Validator runs official ones.
    /// Records a backtest run. Returns false when it doesn't reproduce: an earlier run of this
    /// version had the same inputs and different results. A non-reproducing official run is an
    /// automatic FAIL (spec §11): the strategy fails back to SPEC_FROZEN and a new version, so the
    /// code must be submitted again, and the VALIDATED gate stays shut for this version anyway.
    pub async fn record_backtest(
        &self,
        agent_id: &str,
        strategy: i64,
        official: bool,
        inputs_hash: &str,
        results_hash: &str,
    ) -> Result<bool> {
        plain("inputs hash", inputs_hash)?;
        plain("results hash", results_hash)?;
        let action = if official {
            Action::RunOfficialBacktest
        } else {
            Action::RunDevBacktest
        };
        self.authorize(agent_id, action).await?;
        let mut tx = self.pool.begin().await?;
        let (reproduces, _) = record_backtest_in(
            &mut tx,
            agent_id,
            strategy,
            official,
            inputs_hash,
            results_hash,
        )
        .await?;
        tx.commit().await?;
        Ok(reproduces)
    }
}

/// `record_backtest`'s work inside the caller's transaction. Returns whether it reproduces and
/// the version the run was for (before any fail back bumps it).
async fn record_backtest_in(
    tx: &mut Transaction<'_, Sqlite>,
    agent_id: &str,
    strategy: i64,
    official: bool,
    inputs_hash: &str,
    results_hash: &str,
) -> Result<(bool, i64)> {
    {
        let version = version_in(tx, strategy).await?;
        let differing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM backtest_runs WHERE strategy_id = ? AND version = ? AND inputs_hash = ? AND results_hash <> ?",
        )
        .bind(strategy)
        .bind(version)
        .bind(inputs_hash)
        .bind(results_hash)
        .fetch_one(&mut **tx)
        .await?;
        let reproduces = differing == 0;
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
        .execute(&mut **tx)
        .await?;
        append(
            tx,
            agent_id,
            "record_backtest",
            json!({"strategy": strategy, "version": version, "official": official, "inputs": inputs_hash, "results": results_hash, "reproduces": reproduces}),
        )
        .await?;
        if official && !reproduces {
            let state: String = sqlx::query_scalar("SELECT state FROM strategies WHERE id = ?")
                .bind(strategy)
                .fetch_one(&mut **tx)
                .await?;
            let state = State::parse(&state)
                .ok_or_else(|| DeskError::Corrupt(format!("stored state {state:?}")))?;
            // only a strategy past SPEC_FROZEN that can step back does; one that can't (halted,
            // retired, not yet coded) keeps its state, the run is still recorded, and the
            // VALIDATED gate still refuses this version
            let can_fail_back = lifecycle::rule(state, State::SpecFrozen, None)
                .is_some_and(|r| r.kind == Kind::Back);
            if can_fail_back {
                let back = vec![Signer {
                    agent_id: agent_id.into(),
                    proof: None,
                }];
                transition_in(tx, strategy, State::SpecFrozen, back).await?;
            }
            append(
                tx,
                agent_id,
                "reproduction_fail",
                json!({"strategy": strategy, "version": version, "inputs": inputs_hash}),
            )
            .await?;
        }
        Ok((reproduces, version))
    }
}

impl Store {
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
        transition_in(&mut tx, strategy, to, signers).await?;
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

/// A lifecycle move inside the caller's transaction; nothing is written unless every check passes.
async fn transition_in(
    tx: &mut Transaction<'_, Sqlite>,
    strategy: i64,
    to: State,
    signers: Vec<Signer>,
) -> Result<()> {
    let mut sigs = Vec::new();
    for s in signers {
        let role = role_in(&mut *tx, &s.agent_id).await?;
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
    .fetch_optional(&mut **tx)
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
            .fetch_one(&mut **tx)
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
    .fetch_optional(&mut **tx)
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
    let author_role = role_in(&mut *tx, &author_id).await?;
    // the evidence each forward gate needs
    let missing = match to {
        State::SpecFrozen if criteria.is_none() => Some("pre-registered criteria"),
        State::Implemented if code_author.is_none() => Some("submitted code for this version"),
        State::Validated if !official_run(&mut *tx, strategy, version).await? => {
            Some("an official backtest of this version that reproduces")
        }
        State::LimitsSet => {
            let n: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM risk_limits WHERE strategy_id = ?")
                    .bind(strategy)
                    .fetch_one(&mut **tx)
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
    .fetch_optional(&mut **tx)
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
    sqlx::query(
        "UPDATE strategies SET state = ?, version = ?, current_halt = ? WHERE id = ? AND state = ?",
    )
    .bind(to.as_str())
    .bind(new_version)
    .bind(halt)
    .bind(strategy)
    .bind(from.as_str())
    .execute(&mut **tx)
    .await?;
    let t = sqlx::query(
        "INSERT INTO state_transitions (strategy_id, from_state, to_state, version, at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(strategy)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(new_version)
    .bind(now())
    .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await?;
    }
    let signed: Vec<_> = sigs
        .iter()
        .map(|s| json!({"agent": s.agent_id, "role": s.role.as_str(), "human": s.human}))
        .collect();
    append(
        &mut *tx,
        &sigs[0].agent_id,
        "transition",
        json!({"strategy": strategy, "from": from.as_str(), "to": to.as_str(), "version": new_version, "signers": signed}),
    )
    .await?;
    Ok(())
}

async fn issue_token_in(tx: &mut Transaction<'_, Sqlite>, agent_id: &str) -> Result<String> {
    role_in(&mut *tx, agent_id).await?;
    let token = format!("desk_{}", random_hex(32)?);
    let hash = sha256(&token);
    sqlx::query("DELETE FROM agent_tokens WHERE agent_id = ?")
        .bind(agent_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO agent_tokens (token_sha256, agent_id, created) VALUES (?, ?, ?)")
        .bind(&hash)
        .bind(agent_id)
        .bind(now())
        .execute(&mut **tx)
        .await?;
    append(
        &mut *tx,
        "board",
        "issue_token",
        json!({"agent": agent_id, "token_sha256": hash}),
    )
    .await?;
    Ok(token)
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
    // any two runs of this version with the same inputs and different results: it doesn't reproduce
    let mismatched: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM backtest_runs a JOIN backtest_runs b
           ON a.strategy_id = b.strategy_id AND a.version = b.version
          AND a.inputs_hash = b.inputs_hash AND a.results_hash <> b.results_hash
         WHERE a.strategy_id = ? AND a.version = ?",
    )
    .bind(strategy)
    .bind(version)
    .fetch_one(&mut **tx)
    .await?;
    Ok(n > 0 && mismatched == 0)
}

fn peak_account(account: &str) -> Result<()> {
    match account {
        "paper" | "live" => Ok(()),
        _ => Err(DeskError::NotFound(format!("account {account:?}"))),
    }
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
        s.bootstrap_owner("test-key").await.unwrap();
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
    async fn a_charge_is_always_booked_and_then_blocks() {
        let s = desk().await;
        assert_eq!(s.budget("val-1").await.unwrap(), None);
        assert!(
            s.charge("val-1", d("0.01")).await.is_err(),
            "no budget, no charge"
        );
        s.set_budget("board", "val-1", d("1.00"), proof())
            .await
            .unwrap();
        s.charge("val-1", d("0.70")).await.unwrap();
        // past the limit: still booked, so the books are exact
        s.charge("val-1", d("0.70")).await.unwrap();
        assert_eq!(
            s.budget("val-1").await.unwrap(),
            Some((d("1.00"), d("1.40")))
        );
        assert!(s.charge("val-1", d("-1")).await.is_err(), "no refunds");
        assert!(matches!(
            s.authorize("val-1", Action::RunOfficialBacktest).await,
            Err(DeskError::Budget(_))
        ));
    }

    #[tokio::test]
    async fn only_the_engine_posts_evidence_and_the_desk_checks_the_criteria() {
        let s = desk().await;
        s.add_agent("board", "engine-1", Role::Engine, Some(proof()))
            .await
            .unwrap();
        s.add_agent("board", "hod-1", Role::HeadOfDesk, Some(proof()))
            .await
            .unwrap();
        let id = s.create_strategy("res-1", "drift").await.unwrap();
        s.transition(id, State::HypothesisApproved, by("strat-1"))
            .await
            .unwrap();
        let criteria = r#"{"checks": [{"metric": "deflated_sharpe", "min": 0.95}, {"metric": "expectancy", "min": 0.02}]}"#;
        s.set_criteria("strat-1", id, criteria).await.unwrap();
        s.transition(id, State::SpecFrozen, by("po-1"))
            .await
            .unwrap();
        s.submit_version("dev-1", id, "sha256:abc").await.unwrap();
        s.transition(id, State::Implemented, by("strat-1"))
            .await
            .unwrap();
        let body = json!({"metrics": {"deflated_sharpe": 0.97, "expectancy": 0.01}, "source": "stress-battery"});

        // no agent can post numbers, the Validator included
        for agent in ["val-1", "stress-1", "hod-1", "dev-1"] {
            assert!(
                s.post_evidence(agent, id, "in", &body).await.is_err(),
                "{agent}"
            );
        }
        assert!(
            s.post_evidence("engine-1", id, "in", &body).await.unwrap(),
            "reproduces"
        );

        let v = s.evidence("val-1", id).await.unwrap();
        let e = &v["evidence"];
        assert_eq!(
            e["results_hash"],
            crate::evidence::hash(&body),
            "the desk's own hash"
        );
        assert_eq!(e["criteria_checks"][0]["passed"], true);
        assert_eq!(
            e["criteria_checks"][1]["passed"], false,
            "expectancy under the bar"
        );
        assert_eq!(e["all_criteria_pass"], false);
        assert!(
            s.evidence("hod-1", id).await.is_ok(),
            "the Head of Desk reads it"
        );
        assert!(
            s.evidence("res-1", id).await.is_err(),
            "the Researcher doesn't"
        );

        // the same inputs with different numbers don't reproduce: an automatic fail back
        let other = json!({"metrics": {"deflated_sharpe": 0.5, "expectancy": 0.01}});
        assert!(!s.post_evidence("engine-1", id, "in", &other).await.unwrap());
        assert_eq!(s.state(id).await.unwrap(), State::SpecFrozen);
        // the failed run's body stays on the version it was for; the new version has none yet
        let per_version: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM evidence WHERE strategy_id = ? ORDER BY id")
                .bind(id)
                .fetch_all(&s.pool)
                .await
                .unwrap();
        assert_eq!(per_version, [(1,), (1,)]);
        let after = s.evidence("val-1", id).await.unwrap();
        assert_eq!(after["version"], 2);
        assert!(
            after["evidence"].is_null(),
            "nothing yet for the new version"
        );
        // the engine never signs by hand, not even a step back
        assert!(denied(
            s.sign("engine-1", id, State::Implemented).await,
            "doesn't sign"
        ));
        assert!(
            s.set_criteria(
                "strat-1",
                id,
                &"x".repeat(crate::evidence::MAX_CRITERIA + 1)
            )
            .await
            .is_err(),
            "criteria are capped"
        );
        assert!(
            s.post_evidence("engine-1", id, "in", &json!({"metrics": {}}))
                .await
                .is_err(),
            "a body with no numbers is refused"
        );
    }

    #[tokio::test]
    async fn an_existing_database_gets_the_evidence_table_and_its_guards() {
        let path = std::env::temp_dir().join(format!("bmv-desk-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite://{}", path.display());
        let s = Store::open(&url).await.unwrap();
        // a database from before S46: no evidence table, no guards on it
        sqlx::query("DROP TABLE evidence")
            .execute(&s.pool)
            .await
            .unwrap();
        drop(s);
        let s = Store::open(&url).await.unwrap();
        let found: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE tbl_name = 'evidence' ORDER BY name",
        )
        .fetch_all(&s.pool)
        .await
        .unwrap();
        assert!(found.iter().any(|(n,)| n == "evidence"), "{found:?}");
        assert!(
            found.len() >= 3,
            "the table and its no-update and no-delete guards: {found:?}"
        );
        drop(s);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn an_official_run_that_does_not_reproduce_fails_the_strategy_back() {
        let s = desk().await;
        let id = implemented(&s).await;
        let v = s.version(id).await.unwrap();
        assert!(
            s.record_backtest("dev-1", id, false, "in1", "out1")
                .await
                .unwrap()
        );
        // same inputs, same results: it reproduces, and the gate can open
        assert!(
            s.record_backtest("val-1", id, true, "in1", "out1")
                .await
                .unwrap()
        );

        let id2 = implemented(&s).await;
        let v2 = s.version(id2).await.unwrap();
        s.record_backtest("dev-1", id2, false, "in2", "out2")
            .await
            .unwrap();
        assert!(
            !s.record_backtest("val-1", id2, true, "in2", "other")
                .await
                .unwrap()
        );
        assert_eq!(s.state(id2).await.unwrap(), State::SpecFrozen);
        assert_eq!(s.version(id2).await.unwrap(), v2 + 1, "a new version");
        assert!(
            s.transition(id2, State::Implemented, by("strat-1"))
                .await
                .is_err(),
            "the code must be submitted again"
        );
        let trail = s.audit_entries().await.unwrap();
        assert!(trail.iter().any(|e| e.action == "reproduction_fail"));

        // a mismatched pair keeps the gate shut even if nothing could move the strategy back
        let id3 = implemented(&s).await;
        s.record_backtest("val-1", id3, true, "in3", "a")
            .await
            .unwrap();
        s.record_backtest("dev-1", id3, false, "in3", "b")
            .await
            .unwrap();
        assert!(denied(
            s.transition(id3, State::Validated, by("val-1")).await,
            "reproduces"
        ));
        s.transition(id, State::Validated, by("val-1"))
            .await
            .unwrap();
        assert_eq!(s.version(id).await.unwrap(), v);

        // before any code there is nothing to fail back, but the run is still recorded
        let early = s.create_strategy("res-1", "early idea").await.unwrap();
        s.transition(early, State::HypothesisApproved, by("strat-1"))
            .await
            .unwrap();
        s.record_backtest("dev-1", early, false, "in4", "x")
            .await
            .unwrap();
        assert!(
            !s.record_backtest("val-1", early, true, "in4", "y")
                .await
                .unwrap()
        );
        assert_eq!(s.state(early).await.unwrap(), State::HypothesisApproved);
        s.verify_audit().await.unwrap();
    }

    #[tokio::test]
    async fn tokens_map_to_agents_and_cosigners_collect() {
        let s = desk().await;
        let t = s.issue_token("val-1").await.unwrap();
        assert_eq!(s.agent_for_token(&t).await.unwrap(), "val-1");
        assert!(s.agent_for_token("desk_nope").await.is_err());
        let stored: String = sqlx::query_scalar("SELECT token_sha256 FROM agent_tokens")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_ne!(stored, t, "only the hash is kept");

        let id = paper(&s).await;
        s.record_paper("ops-1", id, 21, 33).await.unwrap();
        assert!(s.sign("board", id, State::LiveApproved).await.is_err());
        assert!(
            denied(
                s.sign("comp-1", id, State::PaperReviewed).await,
                "doesn't sign"
            ),
            "a role the step doesn't need isn't kept"
        );
        assert!(!s.sign("po-1", id, State::PaperReviewed).await.unwrap());
        assert!(!s.sign("po-1", id, State::PaperReviewed).await.unwrap());
        assert_eq!(s.state(id).await.unwrap(), State::PaperTrading);
        assert!(s.sign("risk-1", id, State::PaperReviewed).await.unwrap());
        assert_eq!(s.state(id).await.unwrap(), State::PaperReviewed);
        s.verify_audit().await.unwrap();
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

    #[tokio::test]
    async fn the_peak_only_rises_and_only_the_engine_raises_it() {
        let s = desk().await;
        s.add_agent("board", "engine-1", Role::Engine, Some(proof()))
            .await
            .unwrap();
        assert_eq!(s.peak("engine-1", "paper").await.unwrap(), None);
        assert_eq!(
            s.raise_peak("engine-1", "paper", 2_500_000).await.unwrap(),
            2_500_000
        );
        // lower or equal keeps the peak, writes nothing
        assert_eq!(
            s.raise_peak("engine-1", "paper", 2_400_000).await.unwrap(),
            2_500_000
        );
        assert_eq!(
            s.raise_peak("engine-1", "paper", 2_500_000).await.unwrap(),
            2_500_000
        );
        assert_eq!(
            s.raise_peak("engine-1", "paper", 2_600_000).await.unwrap(),
            2_600_000
        );
        let (cents, _) = s.peak("risk-1", "paper").await.unwrap().unwrap();
        assert_eq!(cents, 2_600_000);
        // accounts are kept apart
        assert_eq!(s.peak("engine-1", "live").await.unwrap(), None);
        assert!(matches!(
            s.peak("engine-1", "margin").await,
            Err(DeskError::NotFound(_))
        ));
        assert!(matches!(
            s.raise_peak("engine-1", "paper", 0).await,
            Err(DeskError::Denied(_))
        ));
        // no agent but the engine raises it, and readers are limited
        for who in ["ops-1", "risk-1", "board", "dev-1"] {
            assert!(
                matches!(
                    s.raise_peak(who, "paper", 9_000_000).await,
                    Err(DeskError::Denied(_))
                ),
                "{who}"
            );
        }
        assert!(matches!(
            s.peak("dev-1", "paper").await,
            Err(DeskError::Denied(_))
        ));
        // even raw SQL can't lower, change or delete it
        for sql in [
            "INSERT INTO equity_peaks (account, cents, agent_id, at) VALUES ('paper', 100, 'engine-1', 'x')",
            "UPDATE equity_peaks SET cents = 1",
            "DELETE FROM equity_peaks",
        ] {
            assert!(sqlx::raw_sql(sql).execute(&s.pool).await.is_err(), "{sql}");
        }
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM equity_peaks")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert!(s.verify_audit().await.is_ok());
    }
}
