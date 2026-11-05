//! Every key the system holds, in one place (DECISION S43). `bmv-trader keys` (via
//! `scripts/keys.sh`) shows each one's purpose, renewal date and state; `--notify` (launchd, daily)
//! posts a notification when any is 10 days or less from due; `--renew` walks through the due
//! ones: what the key is, where to get a new one, a hidden paste, a test, then saving it and
//! updating everything that uses it.
//!
//! - **Values** are never printed, logged or put in argv. A pasted key is tested before it is saved,
//!   so a wrong paste changes nothing.
//! - **Dates** for keys renewed by hand live in `data/keys.json`; automatic keys report from their
//!   own logs (`data/free_keys.jsonl`, `data/llm_rotated.json`).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a key lives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Store {
    Keychain {
        service: &'static str,
        account: &'static str,
    },
    /// Rotated by its own job; this module only reports on it.
    Automatic,
    /// Never expires; listed so the table is complete.
    Lasting,
}

/// What a renewed key needs after it is saved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum After {
    Nothing,
    /// Copied into every desk agent's keys file that should hold it (`clip-key-swap.sh`).
    AgentFiles(&'static str),
}

/// How a pasted key is proven before it is saved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Test {
    /// A free call to Anthropic's models list.
    Anthropic,
    /// A list of the account's keys, which only a management key can make.
    OpenRouterManagement,
    /// No outside test (OmniRoute's local key): saved as given.
    None,
}

/// One kind of key the system holds.
pub struct Def {
    pub id: &'static str,
    pub name: &'static str,
    pub purpose: &'static str,
    /// What stops working if it lapses.
    pub breaks: &'static str,
    pub store: Store,
    /// `keys` section field in `deploy/models.json` for its renewal interval, in days.
    pub every: Option<&'static str>,
    pub how: &'static [&'static str],
    pub test: Test,
    pub after: After,
    /// How to retire the old one once the new one works.
    pub old: &'static str,
    /// Tracked only when its Keychain item exists.
    pub optional: bool,
}

pub const DEFS: &[Def] = &[
    Def {
        id: "claude-engine",
        name: "Claude engine key",
        purpose: "trial 10's analyst and the daily notes",
        breaks: "trial 10 stops analysing signals and the notes fall back to numbers only",
        store: Store::Keychain {
            service: "bmv-trader",
            account: "ANTHROPIC_API_KEY",
        },
        every: Some("claude_every_days"),
        how: &[
            "Open console.anthropic.com and sign in as an admin.",
            "Settings, API Keys; pick the engine's workspace (the one trial 10 uses).",
            "Click Create Key, name it bmv-engine-<month>, and copy it (shown once).",
        ],
        test: Test::Anthropic,
        after: After::Nothing,
        old: "In the same workspace, disable the previous bmv-engine-* key (the older Created date); delete it a month later.",
        optional: false,
    },
    Def {
        id: "claude-desk",
        name: "Claude desk key",
        purpose: "the four desk agents (Head of Desk, Researcher, Validator, Stress Lead)",
        breaks: "the desk agents' runs fail before any paid call",
        store: Store::Keychain {
            service: "bmv-desk-models",
            account: "anthropic-desk",
        },
        every: Some("claude_every_days"),
        how: &[
            "Open console.anthropic.com and sign in as an admin.",
            "Settings, API Keys; pick the desk workspace (the one with the hard monthly limit).",
            "Click Create Key, name it bmv-desk-<month>, and copy it (shown once).",
        ],
        test: Test::Anthropic,
        after: After::AgentFiles("anthropic"),
        old: "Once every agent has it, disable the previous bmv-desk-* key in the desk workspace.",
        optional: false,
    },
    Def {
        id: "openrouter-management",
        name: "OpenRouter management key",
        purpose: "makes the weekly free-model key (it can't call any model itself)",
        breaks: "the weekly free-model key stops rotating; the current one keeps working until it expires",
        store: Store::Keychain {
            service: "bmv-trader",
            account: "OPENROUTER_MANAGEMENT_KEY",
        },
        every: Some("openrouter_management_every_days"),
        how: &[
            "Open openrouter.ai/settings/management-keys and sign in.",
            "Click Create New Key, name it bmv-rotation, set the expiry to 90 days.",
            "Copy it (shown once).",
        ],
        test: Test::OpenRouterManagement,
        after: After::Nothing,
        old: "On the same page, delete the previous bmv-rotation key.",
        optional: false,
    },
    Def {
        id: "omniroute-agents",
        name: "OmniRoute agents key",
        purpose: "lets the Head of Desk and Researcher use the free route if switched to it",
        breaks: "an agent switched to its free model can't run",
        store: Store::Keychain {
            service: "bmv-desk-models",
            account: "omniroute-agents",
        },
        every: Some("omniroute_agents_every_days"),
        how: &[
            "Open the OmniRoute dashboard at http://127.0.0.1:20128, API keys.",
            "Regenerate the desk agents' key; it must allow only the bmv-free route, at cost 0.",
            "Copy it.",
        ],
        test: Test::None,
        after: After::AgentFiles("omniroute"),
        old: "Regenerating already retired the old one.",
        optional: true,
    },
    Def {
        id: "openrouter-model",
        name: "OpenRouter model key",
        purpose: "the free models behind the bmv-free route",
        breaks: "the free route stops; lab ideas are skipped",
        store: Store::Automatic,
        every: None,
        how: &[],
        test: Test::None,
        after: After::Nothing,
        old: "",
        optional: false,
    },
    Def {
        id: "omniroute-engine",
        name: "OmniRoute engine key",
        purpose: "the engine's calls to the free route",
        breaks: "lab ideas are skipped",
        store: Store::Automatic,
        every: None,
        how: &[],
        test: Test::None,
        after: After::Nothing,
        old: "",
        optional: false,
    },
    Def {
        id: "desk-tokens",
        name: "Desk agent tokens",
        purpose: "each agent's login to the desk",
        breaks: "",
        store: Store::Lasting,
        every: None,
        how: &[],
        test: Test::None,
        after: After::Nothing,
        old: "",
        optional: false,
    },
    Def {
        id: "alpaca",
        name: "Alpaca keys",
        purpose: "paper trading",
        breaks: "",
        store: Store::Lasting,
        every: None,
        how: &[],
        test: Test::None,
        after: After::Nothing,
        old: "",
        optional: false,
    },
];

pub fn def(id: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.id == id)
}

/// The `keys` section of `deploy/models.json`: renewal intervals and the warning window, in days.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub warn_days: i64,
    pub claude_every_days: i64,
    pub openrouter_management_every_days: i64,
    pub omniroute_agents_every_days: i64,
}

#[derive(Deserialize)]
struct ModelsFile {
    keys: Policy,
}

impl Policy {
    pub fn parse(models_json: &str) -> Result<Policy, String> {
        let p = serde_json::from_str::<ModelsFile>(models_json)
            .map_err(|e| format!("models file, keys: {e}"))?
            .keys;
        for (name, v) in [
            ("claude_every_days", p.claude_every_days),
            (
                "openrouter_management_every_days",
                p.openrouter_management_every_days,
            ),
            ("omniroute_agents_every_days", p.omniroute_agents_every_days),
        ] {
            if !(7..=365).contains(&v) {
                return Err(format!("models file, keys: {name} must be 7 to 365"));
            }
        }
        if !(1..=60).contains(&p.warn_days) {
            return Err("models file, keys: warn_days must be 1 to 60".into());
        }
        Ok(p)
    }

    pub fn load(home: &Path) -> Result<Policy, String> {
        let path = crate::llm::models_path(home);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Policy::parse(&text)
    }

    fn every(&self, field: &str) -> Option<i64> {
        match field {
            "claude_every_days" => Some(self.claude_every_days),
            "openrouter_management_every_days" => Some(self.openrouter_management_every_days),
            "omniroute_agents_every_days" => Some(self.omniroute_agents_every_days),
            _ => None,
        }
    }
}

/// `data/keys.json`: when each key renewed by hand was last renewed, `{id: RFC 3339}`.
pub fn dates_path(home: &Path) -> PathBuf {
    home.join("data").join("keys.json")
}

/// The renewal dates. Missing is empty; one that doesn't parse is an error, never a silent reset.
/// Dates from the older `data/keys_rotated.json` (`engine`, `desk`) fill any gap.
pub fn read_dates(home: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut dates: BTreeMap<String, String> = match std::fs::read(dates_path(home)) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("data/keys.json: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(format!("data/keys.json: {e}")),
    };
    if let Ok(b) = std::fs::read(home.join("data").join("keys_rotated.json"))
        && let Ok(old) = serde_json::from_slice::<BTreeMap<String, String>>(&b)
    {
        for (from, to) in [("engine", "claude-engine"), ("desk", "claude-desk")] {
            if let Some(d) = old.get(from) {
                dates.entry(to.into()).or_insert_with(|| d.clone());
            }
        }
    }
    Ok(dates)
}

/// Records a key renewed today: written beside the file, then renamed over it.
pub fn record(home: &Path, id: &str, now: chrono::DateTime<chrono::Utc>) -> Result<(), String> {
    let d = def(id).ok_or_else(|| format!("no key called {id}; see scripts/keys.sh"))?;
    if !matches!(d.store, Store::Keychain { .. }) {
        return Err(format!("{} renews itself; nothing to record", d.name));
    }
    let mut dates = read_dates(home)?;
    dates.insert(
        id.into(),
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    let path = dates_path(home);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let tmp = path.with_extension("json.new");
    let text = serde_json::to_string_pretty(&dates).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text + "\n").map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

/// A key's state, for the table and the notification.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Ok {
        days_left: i64,
    },
    DueSoon {
        days_left: i64,
    },
    Overdue {
        days: i64,
    },
    /// In the Keychain, but never renewed through this tracker: its age is unknown.
    Unknown,
    Missing,
    /// The Keychain couldn't be asked (locked, timed out, `security` missing): not a missing key.
    Unchecked(String),
    /// An optional key that isn't set up.
    NotUsed,
    Automatic(Result<String, String>),
    Lasting,
}

impl State {
    /// True when it needs the owner.
    pub fn needs_you(&self) -> bool {
        matches!(
            self,
            State::DueSoon { .. }
                | State::Overdue { .. }
                | State::Unknown
                | State::Missing
                | State::Unchecked(_)
                | State::Automatic(Err(_))
        )
    }

    pub fn label(&self) -> String {
        match self {
            State::Ok { days_left } => format!("ok, due in {days_left} days"),
            State::DueSoon { days_left } => format!("RENEW: due in {days_left} days"),
            State::Overdue { days } => format!("RENEW: {days} days overdue"),
            State::Unknown => "RENEW: date unknown".into(),
            State::Missing => "MISSING: not in the Keychain".into(),
            State::Unchecked(e) => format!("UNKNOWN: can't check the Keychain ({e})"),
            State::NotUsed => "not set up (optional)".into(),
            State::Automatic(Ok(s)) => format!("automatic: {s}"),
            State::Automatic(Err(s)) => format!("PROBLEM: {s}"),
            State::Lasting => "no expiry".into(),
        }
    }
}

/// One row of the table.
pub struct Row {
    pub def: &'static Def,
    pub renewed: Option<String>,
    pub due: Option<String>,
    pub state: State,
}

/// What the owner's machine says about each key: whether its Keychain item exists, and the
/// automatic keys' own states.
pub struct Facts<'a> {
    pub present: &'a dyn Fn(&'static str, &'static str) -> Result<bool, String>,
    pub automatic: &'a dyn Fn(&'static str) -> Result<String, String>,
}

/// The table: every key's renewal date, due date and state.
pub fn rows(
    policy: &Policy,
    dates: &BTreeMap<String, String>,
    facts: &Facts,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Row> {
    DEFS.iter()
        .map(|d| {
            let (renewed, due, state) = match d.store {
                Store::Lasting => (None, None, State::Lasting),
                Store::Automatic => (None, None, State::Automatic((facts.automatic)(d.id))),
                Store::Keychain { service, account } => match (facts.present)(service, account) {
                    Err(e) => (None, None, State::Unchecked(e)),
                    Ok(false) => (
                        None,
                        None,
                        if d.optional {
                            State::NotUsed
                        } else {
                            State::Missing
                        },
                    ),
                    Ok(true) => {
                        let every = d.every.and_then(|f| policy.every(f)).unwrap_or(30);
                        // a date in the future is as good as none: it could hide a due key
                        match dates
                            .get(d.id)
                            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                            .filter(|t| t.with_timezone(&chrono::Utc) <= now)
                        {
                            None => (None, None, State::Unknown),
                            Some(t) => {
                                let t = t.with_timezone(&chrono::Utc);
                                let due = t + chrono::Duration::days(every);
                                // whole days, rounding down: a key due in an hour has 0 left
                                let left = (due - now).num_hours().div_euclid(24);
                                let state = if due <= now {
                                    State::Overdue {
                                        days: (now - due).num_days(),
                                    }
                                } else if left < policy.warn_days {
                                    State::DueSoon { days_left: left }
                                } else {
                                    State::Ok { days_left: left }
                                };
                                (
                                    Some(t.format("%Y-%m-%d").to_string()),
                                    Some(due.format("%Y-%m-%d").to_string()),
                                    state,
                                )
                            }
                        }
                    }
                },
            };
            Row {
                def: d,
                renewed,
                due,
                state,
            }
        })
        .collect()
}

pub fn table(rows: &[Row]) -> String {
    let mut out = format!("{:27} {:11} {:11} {}\n", "Key", "Renewed", "Due", "State");
    for r in rows {
        out += &format!(
            "{:27} {:11} {:11} {}\n",
            r.def.name,
            r.renewed.as_deref().unwrap_or("-"),
            r.due.as_deref().unwrap_or("-"),
            r.state.label()
        );
    }
    out
}

/// The table for the console's desk page (S47): names, dates and states only, never where a key
/// is kept.
pub fn json(rows: &[Row]) -> serde_json::Value {
    rows.iter()
        .map(|r| {
            serde_json::json!({
                "id": r.def.id,
                "name": r.def.name,
                "breaks": r.def.breaks,
                "renewed": r.renewed,
                "due": r.due,
                "state": r.state.label(),
                "needs_you": r.state.needs_you(),
            })
        })
        .collect()
}

/// The notification text when anything needs the owner, or None when all is well.
pub fn notice(rows: &[Row]) -> Option<String> {
    let due: Vec<&str> = rows
        .iter()
        .filter(|r| r.state.needs_you())
        .map(|r| r.def.name)
        .collect();
    if due.is_empty() {
        return None;
    }
    Some(format!(
        "{} need{} you: {}. Run scripts/keys.sh renew",
        if due.len() == 1 {
            "1 key".to_string()
        } else {
            format!("{} keys", due.len())
        },
        if due.len() == 1 { "s" } else { "" },
        due.join(", ")
    ))
}

/// The agents that should hold a key of `kind`: no Claude key for one running as Paperclip's own
/// user (S39), no free-route key for a signer (A5). Rows are `desk-agent --list` JSON.
pub fn agents_for(list_json: &str, kind: &str) -> Result<Vec<String>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(list_json).map_err(|e| format!("desk-agent --list: {e}"))?;
    if rows.is_empty() {
        return Err("the agents file lists no agents".into());
    }
    rows.iter()
        .filter(|a| match kind {
            "anthropic" => a["runs_as"].as_str() != Some("bmvclip"),
            "omniroute" => a["signs"].as_bool() != Some(true),
            _ => false,
        })
        .map(|a| {
            a["id"]
                .as_str()
                .map(String::from)
                .ok_or_else(|| "desk-agent --list: an agent with no id".to_string())
        })
        .collect()
}

/// The Keychain item of the other Claude key, which a renewed one must never equal: the engine
/// and desk keys live in different workspaces with their own spending limits.
pub fn sibling(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "claude-engine" => Some(("bmv-desk-models", "anthropic-desk")),
        "claude-desk" => Some(("bmv-trader", "ANTHROPIC_API_KEY")),
        _ => None,
    }
}

/// A pasted key may only hold the characters every provider's keys use, so it can never break
/// the Keychain command's quoting.
pub fn plausible(key: &str) -> bool {
    (20..=300).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::parse(include_str!("../deploy/models.json")).unwrap()
    }

    fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().into()
    }

    fn state<'a>(rows: &'a [Row], id: &str) -> &'a State {
        &rows.iter().find(|r| r.def.id == id).unwrap().state
    }

    #[test]
    fn every_key_is_listed_once_and_the_policy_is_valid() {
        let p = policy();
        assert_eq!((p.warn_days, p.claude_every_days), (10, 30));
        let mut ids: Vec<_> = DEFS.iter().map(|d| d.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), DEFS.len());
        for d in DEFS {
            if let Store::Keychain { .. } = d.store {
                assert!(!d.how.is_empty() && d.every.is_some(), "{}", d.id);
            }
        }
    }

    #[test]
    fn states_follow_the_dates_and_warn_ten_days_ahead() {
        let p = policy();
        let mut dates = BTreeMap::new();
        dates.insert(
            "claude-engine".to_string(),
            "2026-11-01T00:00:00Z".to_string(),
        );
        dates.insert(
            "claude-desk".to_string(),
            "2026-10-01T00:00:00Z".to_string(),
        );
        let present = |_: &'static str, a: &'static str| Ok(a != "omniroute-agents");
        let automatic = |id: &'static str| {
            if id == "omniroute-engine" {
                Err("rotation failed".into())
            } else {
                Ok("fine".into())
            }
        };
        let facts = Facts {
            present: &present,
            automatic: &automatic,
        };
        let rows = rows(&p, &dates, &facts, t("2026-11-15T00:00:00Z"));
        assert_eq!(state(&rows, "claude-engine"), &State::Ok { days_left: 16 });
        assert_eq!(state(&rows, "claude-desk"), &State::Overdue { days: 15 });
        assert_eq!(
            state(&rows, "openrouter-management"),
            &State::Unknown,
            "in the Keychain, no date"
        );
        assert_eq!(
            state(&rows, "omniroute-agents"),
            &State::NotUsed,
            "optional and not set up"
        );
        let mut future = dates.clone();
        future.insert(
            "claude-engine".to_string(),
            "2027-01-01T00:00:00Z".to_string(),
        );
        let f = super::rows(&p, &future, &facts, t("2026-11-15T00:00:00Z"));
        assert_eq!(
            state(&f, "claude-engine"),
            &State::Unknown,
            "a future date hides nothing"
        );
        assert_eq!(
            sibling("claude-desk"),
            Some(("bmv-trader", "ANTHROPIC_API_KEY"))
        );
        assert!(state(&rows, "omniroute-engine").needs_you());
        assert_eq!(state(&rows, "alpaca"), &State::Lasting);
        let later = super::rows(&p, &dates, &facts, t("2026-11-21T12:00:00Z"));
        assert_eq!(
            state(&later, "claude-engine"),
            &State::DueSoon { days_left: 9 }
        );
        let n = notice(&later).unwrap();
        assert!(
            n.contains("Claude engine key") && n.contains("scripts/keys.sh renew"),
            "{n}"
        );
    }

    #[test]
    fn nothing_to_say_when_all_is_well() {
        let p = policy();
        let mut dates = BTreeMap::new();
        for id in ["claude-engine", "claude-desk", "openrouter-management"] {
            dates.insert(id.to_string(), "2026-11-10T00:00:00Z".to_string());
        }
        let present = |_: &'static str, a: &'static str| Ok(a != "omniroute-agents");
        let automatic = |_: &'static str| Ok("fine".into());
        let facts = Facts {
            present: &present,
            automatic: &automatic,
        };
        assert!(notice(&rows(&p, &dates, &facts, t("2026-11-12T00:00:00Z"))).is_none());
    }

    #[test]
    fn a_keychain_that_cant_be_asked_is_not_a_missing_key() {
        let present = |_: &'static str, _: &'static str| Err("security timed out".to_string());
        let automatic = |_: &'static str| Ok("fine".into());
        let facts = Facts {
            present: &present,
            automatic: &automatic,
        };
        let rows = rows(
            &policy(),
            &BTreeMap::new(),
            &facts,
            t("2026-11-12T00:00:00Z"),
        );
        let s = state(&rows, "claude-engine");
        assert_eq!(s, &State::Unchecked("security timed out".into()));
        assert!(s.needs_you() && s.label().contains("can't check the Keychain"));
        let j = json(&rows);
        assert_eq!(j[0]["needs_you"], true);
        assert!(
            !j.to_string().contains("ANTHROPIC_API_KEY"),
            "never where a key is kept"
        );
    }

    #[test]
    fn recorded_dates_round_trip_and_old_dates_fill_gaps() {
        let home = std::env::temp_dir().join(format!("bmv-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("data")).unwrap();
        std::fs::write(
            home.join("data/keys_rotated.json"),
            r#"{"engine": "2026-11-01T00:00:00Z"}"#,
        )
        .unwrap();
        record(&home, "openrouter-management", t("2026-11-05T18:00:00Z")).unwrap();
        let d = read_dates(&home).unwrap();
        assert_eq!(d["openrouter-management"], "2026-11-05T18:00:00Z");
        assert_eq!(
            d["claude-engine"], "2026-11-01T00:00:00Z",
            "from the older file"
        );
        assert!(
            record(&home, "openrouter-model", t("2026-11-05T18:00:00Z")).is_err(),
            "automatic"
        );
        assert!(record(&home, "nobody", t("2026-11-05T18:00:00Z")).is_err());
        std::fs::write(dates_path(&home), "not json").unwrap();
        assert!(
            read_dates(&home).is_err(),
            "a damaged file is an error, not a reset"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn keys_go_only_to_the_agents_that_may_hold_them() {
        let list = r#"[{"id":"hod-1","runs_as":"bmvhod","signs":false},
                      {"id":"val-1","runs_as":"bmvval","signs":true},
                      {"id":"old-1","runs_as":"bmvclip","signs":false}]"#;
        assert_eq!(agents_for(list, "anthropic").unwrap(), ["hod-1", "val-1"]);
        assert_eq!(agents_for(list, "omniroute").unwrap(), ["hod-1", "old-1"]);
        assert!(
            agents_for("[]", "anthropic").is_err(),
            "an empty list is an error, not 'nothing to do'"
        );
    }

    #[test]
    fn pasted_keys_must_look_like_keys() {
        assert!(plausible("sk-ant-api03-AbC_123-xyzXYZ0123456789"));
        assert!(!plausible("short"));
        assert!(!plausible("sk-ant \"; rm -rf ~"));
    }
}
