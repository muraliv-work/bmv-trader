//! The monitors' shared view (`watch` in the terminal, `ui` in the browser): today's journal,
//! Alpaca's account, the STOP file and the engine heartbeat. Read-only: nothing here talks to the
//! engine or places or cancels anything (DESIGN §3: no buy/sell buttons in any UI).

use crate::broker::Alpaca;
use crate::{live, safety};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct Line {
    pub ts: i64,
    pub kind: String,
    pub id: String,
    pub detail: String,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Summary {
    pub session: Option<String>,
    pub halted: Option<String>,
    pub kill_switch: bool,
    pub unresolved: BTreeSet<String>,
    pub candidates: usize,
    pub rejections: usize,
    pub orders: usize,
    pub fills: usize,
    pub exits: usize,
    /// Lines that didn't parse: shown, never skipped silently.
    pub unreadable: usize,
    pub recent: Vec<Line>,
}

/// Today's state from the journal: only lines from the last SessionStart on.
pub fn summarize(text: &str, keep: usize) -> Summary {
    let mut s = Summary::default();
    let mut lines = Vec::new();
    for raw in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Line>(raw) {
            Ok(l) if l.kind == "SessionStart" => {
                // a new session resets everything but the unreadable count
                s = Summary {
                    session: Some(l.detail.clone()),
                    unreadable: s.unreadable,
                    ..Summary::default()
                };
                lines.clear();
                lines.push(l);
            }
            Ok(l) => {
                match l.kind.as_str() {
                    "Halt" | "Unexpected" => s.halted = s.halted.or(Some(l.detail.clone())),
                    "KillSwitch" => s.kill_switch = l.detail == "on",
                    "Unresolved" => {
                        s.unresolved.insert(l.id.clone());
                    }
                    "Resolved" => {
                        s.unresolved.remove(&l.id);
                    }
                    "Candidate" => s.candidates += 1,
                    "Rejection" => s.rejections += 1,
                    "OrderSubmit" => s.orders += 1,
                    "Fill" => s.fills += 1,
                    "Exit" => s.exits += 1,
                    _ => {}
                }
                lines.push(l);
            }
            Err(_) => s.unreadable += 1,
        }
    }
    let skip = lines.len().saturating_sub(keep);
    s.recent = lines.into_iter().skip(skip).collect();
    s
}

impl Summary {
    /// One word for the top line; the worst state wins.
    pub fn state(&self) -> &'static str {
        if self.session.is_none() {
            "NO SESSION TODAY"
        } else if self.kill_switch {
            "STOPPED (kill switch)"
        } else if self.halted.is_some() {
            "HALTED"
        } else if !self.unresolved.is_empty() {
            "PAUSED (unresolved)"
        } else {
            "TRADING"
        }
    }
}

/// Everything both monitors show, read fresh. Money is preformatted text; nothing from config
/// but the mode. Each Alpaca read fails on its own into `errors`.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub now: i64,
    pub time_et: String,
    pub mode: String,
    pub state: &'static str,
    /// Why STOP reads engaged, straight from the file check.
    pub stop: Option<String>,
    pub summary: Summary,
    pub account: Option<AccountView>,
    pub positions: Option<Vec<PositionView>>,
    pub orders: Option<Vec<OrderView>>,
    pub trials: Vec<Trial>,
    pub errors: BTreeMap<&'static str, String>,
}

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub equity: String,
    pub today_pnl: String,
    pub buying_power: String,
}

#[derive(Debug, Serialize)]
pub struct PositionView {
    pub symbol: String,
    pub qty: i64,
    pub avg_entry_price: String,
}

#[derive(Debug, Serialize)]
pub struct OrderView {
    pub client_order_id: String,
    pub symbol: String,
    pub side: String,
    pub status: String,
    pub qty: u32,
    pub filled_qty: u32,
}

/// One line of data/trials.jsonl; other fields are ignored.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct Trial {
    pub when: String,
    pub strategy: String,
    pub period: String,
    pub trades: u64,
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub ci_low: f64,
}

pub fn snapshot(a: &Alpaca, home: &Path, mode: &str, keep: usize) -> Snapshot {
    let now = chrono::Utc::now().timestamp();
    let mut errors = BTreeMap::new();
    let journal = match std::fs::read_to_string(live::journal_path(home)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            errors.insert("journal", e.to_string());
            String::new()
        }
    };
    let summary = summarize(&journal, keep);
    let stop = match safety::kill_switch(&safety::stop_path(home)) {
        safety::KillSwitch::Clear => None,
        safety::KillSwitch::Engaged(why) => Some(why),
    };
    let trials = match std::fs::read_to_string(home.join("data").join("trials.jsonl")) {
        Ok(t) => parse_trials(&t, &mut errors),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            errors.insert("trials", e.to_string());
            Vec::new()
        }
    };
    fn read<T>(
        errors: &mut BTreeMap<&'static str, String>,
        name: &'static str,
        r: Result<T, String>,
    ) -> Option<T> {
        r.map_err(|e| errors.insert(name, e)).ok()
    }
    let account = read(&mut errors, "account", a.account()).map(|acct| AccountView {
        equity: acct.equity.to_string(),
        today_pnl: (acct.equity - acct.last_equity).to_string(),
        buying_power: acct.buying_power.to_string(),
    });
    let positions = read(&mut errors, "positions", a.positions()).map(|ps| {
        ps.into_iter()
            .map(|p| PositionView {
                symbol: p.symbol,
                qty: p.qty,
                avg_entry_price: p.avg_entry_price.to_string(),
            })
            .collect()
    });
    let orders = read(&mut errors, "orders", live::open_orders(a)).map(|os| {
        os.into_iter()
            .map(|o| OrderView {
                client_order_id: o.client_order_id,
                symbol: o.symbol,
                side: o.side,
                status: o.status,
                qty: o.qty,
                filled_qty: o.filled_qty,
            })
            .collect()
    });
    Snapshot {
        now,
        time_et: et(now),
        mode: mode.to_string(),
        state: state_now(
            &summary,
            &Seen {
                today: &today_et(now),
                stop_file: stop.is_some(),
                journal_ok: !errors.contains_key("journal"),
                heartbeat_age: std::fs::read_to_string(live::heartbeat_path(home))
                    .ok()
                    .and_then(|t| t.trim().parse::<i64>().ok())
                    .map(|t| now - t),
                positions_open: positions.as_ref().is_some_and(|p: &Vec<_>| !p.is_empty()),
            },
        ),
        stop,
        summary,
        account,
        positions,
        orders,
        trials,
        errors,
    }
}

fn parse_trials(text: &str, errors: &mut BTreeMap<&'static str, String>) -> Vec<Trial> {
    let mut bad = 0;
    let trials = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).map_err(|_| bad += 1).ok())
        .collect();
    if bad > 0 {
        errors.insert("trials", format!("{bad} unreadable lines in trials.jsonl"));
    }
    trials
}

/// No heartbeat for this long means the engine isn't running (it ticks every 5 s).
pub const HEARTBEAT_STALE_SECS: i64 = 60;

/// What the monitors know besides the journal.
#[derive(Clone, Copy)]
pub struct Seen<'a> {
    pub today: &'a str,
    pub stop_file: bool,
    pub journal_ok: bool,
    /// Seconds since `run`'s last tick; None if it never wrote one.
    pub heartbeat_age: Option<i64>,
    pub positions_open: bool,
}

/// The worst state wins. A STOP file counts before the engine has logged it, an unreadable journal
/// is unknown, a session from another day is no session, and a session with no live engine says so.
pub fn state_now(s: &Summary, seen: &Seen) -> &'static str {
    let engine_up = seen
        .heartbeat_age
        .is_some_and(|a| (0..HEARTBEAT_STALE_SECS).contains(&a));
    if seen.stop_file {
        "STOPPED (kill switch)"
    } else if !seen.journal_ok {
        "UNKNOWN (journal unreadable)"
    } else if !engine_up && seen.positions_open {
        "ENGINE DOWN, POSITIONS OPEN"
    } else if s.session.as_deref() != Some(seen.today) {
        "NO SESSION TODAY"
    } else if s.halted.is_some() {
        "HALTED"
    } else if !engine_up {
        "ENGINE NOT RUNNING"
    } else {
        s.state()
    }
}

fn today_et(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| {
            t.with_timezone(&chrono_tz::America::New_York)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

/// HH:MM:SS in New York time.
pub fn et(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| {
            t.with_timezone(&chrono_tz::America::New_York)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| format!("ts {ts}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(ts: i64, kind: &str, id: &str, detail: &str) -> String {
        serde_json::json!({"ts": ts, "kind": kind, "id": id, "detail": detail}).to_string()
    }

    #[test]
    fn only_the_latest_session_counts() {
        let text = [
            j(1, "SessionStart", "", "2026-11-02"),
            j(2, "Halt", "", "old halt"),
            j(3, "SessionStart", "", "2026-11-03"),
            j(4, "Candidate", "bmv-2026-11-03-SPY-1", "orb"),
            j(5, "OrderSubmit", "bmv-2026-11-03-SPY-1", ""),
            j(6, "Fill", "bmv-2026-11-03-SPY-1", ""),
        ]
        .join("\n");
        let s = summarize(&text, 10);
        assert_eq!(s.session.as_deref(), Some("2026-11-03"));
        assert_eq!(s.halted, None);
        assert_eq!((s.candidates, s.orders, s.fills), (1, 1, 1));
        assert_eq!(s.recent.len(), 4);
        assert_eq!(s.state(), "TRADING");
    }

    #[test]
    fn worst_state_wins() {
        let base = j(1, "SessionStart", "", "d");
        let s = summarize(
            &[base.clone(), j(2, "Unresolved", "market-data", "")].join("\n"),
            5,
        );
        assert_eq!(s.state(), "PAUSED (unresolved)");
        let s = summarize(
            &[
                base.clone(),
                j(2, "Unresolved", "market-data", ""),
                j(3, "Resolved", "market-data", ""),
            ]
            .join("\n"),
            5,
        );
        assert_eq!(s.state(), "TRADING");
        let s = summarize(
            &[base.clone(), j(2, "Unexpected", "", "short AMD")].join("\n"),
            5,
        );
        assert_eq!(
            (s.state(), s.halted.as_deref()),
            ("HALTED", Some("short AMD"))
        );
        let s = summarize(
            &[
                base.clone(),
                j(2, "Halt", "", "x"),
                j(3, "KillSwitch", "", "on"),
            ]
            .join("\n"),
            5,
        );
        assert_eq!(s.state(), "STOPPED (kill switch)");
        assert_eq!(summarize("", 5).state(), "NO SESSION TODAY");
    }

    #[test]
    fn unreadable_lines_are_counted_and_recent_is_capped() {
        let mut lines = vec![j(1, "SessionStart", "", "d"), "{not json".to_string()];
        lines.extend((2..30).map(|t| j(t, "BarIgnored", "", "")));
        let s = summarize(&lines.join("\n"), 10);
        assert_eq!(s.unreadable, 1);
        assert_eq!(s.recent.len(), 10);
        assert_eq!(s.recent.last().unwrap().ts, 29);
    }

    #[test]
    fn times_are_new_york() {
        assert_eq!(et(1_704_205_800), "09:30:00"); // 2024-01-02 09:30 ET
        assert_eq!(today_et(1_704_205_800), "2024-01-02");
    }

    #[test]
    fn the_worst_seen_state_wins() {
        let s = summarize(&j(1, "SessionStart", "", "2024-01-02"), 5);
        let ok = Seen {
            today: "2024-01-02",
            stop_file: false,
            journal_ok: true,
            heartbeat_age: Some(5),
            positions_open: false,
        };
        assert_eq!(state_now(&s, &ok), "TRADING");
        let st = |seen: Seen| state_now(&s, &seen);
        assert_eq!(
            st(Seen {
                stop_file: true,
                today: "2024-01-03",
                ..ok
            }),
            "STOPPED (kill switch)"
        );
        assert_eq!(
            st(Seen {
                journal_ok: false,
                ..ok
            }),
            "UNKNOWN (journal unreadable)"
        );
        assert_eq!(
            st(Seen {
                today: "2024-01-03",
                ..ok
            }),
            "NO SESSION TODAY"
        );
        for age in [None, Some(60), Some(-5)] {
            assert_eq!(
                st(Seen {
                    heartbeat_age: age,
                    ..ok
                }),
                "ENGINE NOT RUNNING"
            );
            let open = Seen {
                heartbeat_age: age,
                positions_open: true,
                today: "x",
                ..ok
            };
            assert_eq!(st(open), "ENGINE DOWN, POSITIONS OPEN");
        }
        assert_eq!(
            st(Seen {
                positions_open: true,
                ..ok
            }),
            "TRADING"
        );
        let halted = summarize(
            &[
                j(1, "SessionStart", "", "2024-01-02"),
                j(2, "Halt", "", "x"),
            ]
            .join("\n"),
            5,
        );
        assert_eq!(
            state_now(
                &halted,
                &Seen {
                    heartbeat_age: None,
                    ..ok
                }
            ),
            "HALTED"
        );
    }

    #[test]
    fn trials_parse_and_bad_lines_are_reported() {
        let line = r#"{"when":"w","strategy":"orb","params":"p","period":"a..b","trades":3,"expectancy_r":-0.1,"profit_factor":0.9,"max_drawdown_r":5.0,"ci_low":-0.2}"#;
        let mut errors = BTreeMap::new();
        let t = parse_trials(&format!("{line}\nnot json\n"), &mut errors);
        assert_eq!((t.len(), t[0].trades), (1, 3));
        assert!(errors["trials"].starts_with("1 unreadable"));
    }
}
