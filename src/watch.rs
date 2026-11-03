//! Read-only monitor: summarizes today's journal and Alpaca's view. It never talks to the running
//! engine and never places or cancels anything (DESIGN §3: no buy/sell buttons in any UI).

use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Debug, Deserialize, PartialEq)]
pub struct Line {
    pub ts: i64,
    pub kind: String,
    pub id: String,
    pub detail: String,
}

#[derive(Debug, Default, PartialEq)]
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
    }
}
