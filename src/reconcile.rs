//! What reconciliation recorded, and the clean-session count built from it (R13 step 3e, Level A).
//! Each `desk-reconcile` run appends a `Run` to `data/reconcile/runs.jsonl`; the owner logs a
//! `DrillWindow` with `drill-start` before each drill, into `data/reconcile/drill_windows.jsonl`.
//! A session is clean when every scheduled slot ran, every run held a drill position, and every
//! mismatch fell inside a drill window. Any other session sets the count back to 0.

use crate::history::Session;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The count Level A needs.
pub const SESSIONS_NEEDED: usize = 10;
/// The drills a window may be logged for (Level A items 4–6, 9).
pub const CHECKS: [&str; 5] = ["halt", "mismatch", "stop-check", "dead-man", "stop"];
/// launchd runs every 15 minutes from 9:00 ET, 30 minutes before the open.
const SLOT: i64 = 15 * 60;
const FIRST_SLOT_BEFORE_OPEN: i64 = 30 * 60;
/// A slot counts as run if a run started within this long after it (a late launchd start, a run
/// that waited on the network).
const LATE: i64 = 10 * 60;
/// The most of a session drill windows may cover; a longer drill is split across sessions.
pub const WINDOW_MAX_MINUTES: i64 = 120;

/// One reconciliation run. `mismatches` is empty for a clean run; a run that couldn't check the
/// account has one line saying so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// Unix seconds.
    pub at: i64,
    pub drills: usize,
    pub mismatches: Vec<String>,
    /// The halt's outcome, when the run halted.
    pub halt: Option<String>,
}

/// A drill the owner logged before starting it: mismatches and missed runs from `from` to `to`
/// are "drill, expected". `logged_at` is when it was written; `from` is never before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrillWindow {
    pub logged_at: i64,
    pub check: String,
    pub from: i64,
    pub to: i64,
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("data").join("reconcile")
}

pub fn runs_path(home: &Path) -> PathBuf {
    dir(home).join("runs.jsonl")
}

pub fn windows_path(home: &Path) -> PathBuf {
    dir(home).join("drill_windows.jsonl")
}

/// Appends one line and syncs it.
pub fn append<T: Serialize>(path: &Path, row: &T) -> Result<(), String> {
    let line = serde_json::to_string(row).map_err(|e| e.to_string())?;
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(err)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(err)?;
    writeln!(f, "{line}")
        .and_then(|_| f.sync_all())
        .map_err(err)
}

/// Every line; missing is empty, and any unreadable line is an error, never skipped.
pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|e| format!("{} line {}: {e}", path.display(), i + 1))
        })
        .collect()
}

/// A window for `check`, starting now and lasting `minutes`.
pub fn window(check: &str, minutes: i64, now: i64) -> Result<DrillWindow, String> {
    if !CHECKS.contains(&check) {
        return Err(format!(
            "--check must be one of {}, got {check:?}",
            CHECKS.join(", ")
        ));
    }
    if !(1..=WINDOW_MAX_MINUTES).contains(&minutes) {
        return Err(format!(
            "--minutes must be 1 to {WINDOW_MAX_MINUTES}, got {minutes}"
        ));
    }
    Ok(DrillWindow {
        logged_at: now,
        check: check.into(),
        from: now,
        to: now + minutes * 60,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    /// Why not: the session, and what went wrong when.
    NotClean(String),
    /// The session hasn't ended yet.
    Open,
    /// The record started after this session's first slot, so it can't be judged either way.
    Partial,
}

/// The window `t` falls in, if one was logged before it began.
fn window_at(windows: &[DrillWindow], t: i64) -> Option<&DrillWindow> {
    windows
        .iter()
        .find(|w| w.logged_at <= w.from && w.from <= t && t < w.to)
}

fn clock_time(t: i64) -> String {
    use chrono::TimeZone;
    chrono_tz::America::New_York
        .timestamp_opt(t, 0)
        .single()
        .map_or(t.to_string(), |d| d.format("%H:%M ET").to_string())
}

/// The session's scheduled slots: every 15 minutes from 9:00 ET to its close.
fn slots(s: &Session) -> impl Iterator<Item = i64> {
    let first = s.open - FIRST_SLOT_BEFORE_OPEN;
    (0..)
        .map(move |i| first + i * SLOT)
        .take_while(move |t| *t < s.close)
}

fn ran(runs: &[Run], slot: i64) -> bool {
    runs.iter().any(|r| slot <= r.at && r.at < slot + LATE)
}

/// One session's verdict: every slot from 9:00 ET to its close ran (unless inside a drill
/// window), every run held a drill position (inside a window too, except the STOP drill's, which
/// closes it), every mismatch fell inside a window, and windows covered at most
/// `WINDOW_MAX_MINUTES` of it, so no window can stand in for a day of runs.
pub fn session(s: &Session, runs: &[Run], windows: &[DrillWindow], now: i64) -> Verdict {
    if now < s.close + LATE {
        return Verdict::Open;
    }
    let covered = slots(s)
        .filter(|t| window_at(windows, *t).is_some())
        .count() as i64
        * 15;
    if covered > WINDOW_MAX_MINUTES {
        return Verdict::NotClean(format!(
            "{}: drill windows cover {covered} min, over {WINDOW_MAX_MINUTES}",
            s.date
        ));
    }
    for slot in slots(s) {
        if !ran(runs, slot) && window_at(windows, slot).is_none() {
            return Verdict::NotClean(format!("{}: missed the {} run", s.date, clock_time(slot)));
        }
    }
    let first = s.open - FIRST_SLOT_BEFORE_OPEN;
    for r in runs
        .iter()
        .filter(|r| first <= r.at && r.at < s.close + LATE)
    {
        let w = window_at(windows, r.at);
        if !r.mismatches.is_empty() && w.is_none() {
            return Verdict::NotClean(format!(
                "{}: unexplained mismatch at {}: {}",
                s.date,
                clock_time(r.at),
                r.mismatches.join("; ")
            ));
        }
        if r.drills == 0 && w.is_none_or(|w| w.check != "stop") {
            return Verdict::NotClean(format!(
                "{}: no drill position at {} (an empty account proves nothing)",
                s.date,
                clock_time(r.at)
            ));
        }
    }
    Verdict::Clean
}

/// Sessions from the first one the record fully covers, with that session's verdict.
fn judged<'a>(
    sessions: &'a [Session],
    runs: &'a [Run],
    windows: &'a [DrillWindow],
    now: i64,
) -> Result<impl Iterator<Item = (&'a Session, Verdict)>, String> {
    let start = runs.iter().map(|r| r.at).min().unwrap_or(i64::MAX);
    if !runs.is_empty() && !sessions.iter().any(|s| s.close >= start) {
        return Err("no NYSE sessions in the calendar since the first reconcile run".into());
    }
    Ok(sessions
        .iter()
        .filter(move |s| s.close >= start)
        .map(move |s| {
            let v = if s.open - FIRST_SLOT_BEFORE_OPEN + LATE <= start {
                Verdict::Partial
            } else {
                session(s, runs, windows, now)
            };
            (s, v)
        }))
}

/// The clean sessions in a row up to now, counted from the first session the record fully
/// covers, and the last reason the count went back to 0. `sessions` are in date order.
pub fn clean_count(
    sessions: &[Session],
    runs: &[Run],
    windows: &[DrillWindow],
    now: i64,
) -> Result<(usize, Option<String>), String> {
    let (mut n, mut reset) = (0, None);
    for (_, v) in judged(sessions, runs, windows, now)? {
        match v {
            Verdict::Clean => n += 1,
            Verdict::NotClean(why) => {
                n = 0;
                reset = Some(why);
            }
            Verdict::Partial => {}
            Verdict::Open => break,
        }
    }
    Ok((n, reset))
}

/// Scheduled slots with no run and no drill window, in the finished sessions the count judges.
pub fn missed_runs(
    sessions: &[Session],
    runs: &[Run],
    windows: &[DrillWindow],
    now: i64,
) -> Result<usize, String> {
    Ok(judged(sessions, runs, windows, now)?
        .filter(|(_, v)| !matches!(v, Verdict::Partial | Verdict::Open))
        .map(|(s, _)| {
            slots(s)
                .filter(|t| !ran(runs, *t) && window_at(windows, *t).is_none())
                .count()
        })
        .sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-16, a Monday: 9:30 to 16:00 ET is 14:30 to 21:00 UTC
    const OPEN: i64 = 1_763_303_400;
    const CLOSE: i64 = OPEN + 390 * 60;

    fn day(n: i64) -> Session {
        Session {
            date: format!("day{n}"),
            open: OPEN + n * 86_400,
            close: CLOSE + n * 86_400,
        }
    }

    /// A clean run at every slot of the session, 9:00 to 15:45 ET, 20 s after each.
    fn full(s: &Session) -> Vec<Run> {
        (0..28)
            .map(|i| Run {
                at: s.open - 1800 + i * 900 + 20,
                drills: 1,
                mismatches: vec![],
                halt: None,
            })
            .collect()
    }

    const LATER: i64 = CLOSE + 10 * 86_400;

    #[test]
    fn a_full_day_of_clean_runs_with_a_drill_is_clean() {
        let s = day(0);
        assert_eq!(clock_time(s.open), "09:30 ET");
        assert_eq!(session(&s, &full(&s), &[], LATER), Verdict::Clean);
        assert_eq!(session(&s, &full(&s), &[], s.close), Verdict::Open);
    }

    #[test]
    fn a_missed_slot_a_mismatch_or_an_empty_account_is_not_clean() {
        let s = day(0);
        let mut runs = full(&s);
        runs.remove(5); // the 10:15 run
        assert_eq!(
            session(&s, &runs, &[], LATER),
            Verdict::NotClean("day0: missed the 10:15 ET run".into())
        );
        let mut runs = full(&s);
        runs[3].mismatches = vec!["order x: not a drill stop".into()];
        let v = session(&s, &runs, &[], LATER);
        assert!(
            matches!(&v, Verdict::NotClean(w) if w.contains("unexplained mismatch at 09:45 ET")),
            "{v:?}"
        );
        let mut runs = full(&s);
        runs[0].drills = 0;
        let v = session(&s, &runs, &[], LATER);
        assert!(
            matches!(&v, Verdict::NotClean(w) if w.contains("no drill position")),
            "{v:?}"
        );
    }

    #[test]
    fn a_drill_window_logged_first_explains_its_mismatches_and_missed_runs() {
        let s = day(0);
        let mut runs = full(&s);
        runs[3].mismatches = vec!["halt drill".into()];
        runs.remove(4);
        let w = window("halt", 30, s.open + 10 * 60).unwrap(); // 9:40 to 10:10
        assert_eq!(
            session(&s, &runs, std::slice::from_ref(&w), LATER),
            Verdict::Clean
        );
        // a window logged after its start explains nothing
        let late = DrillWindow {
            logged_at: w.from + 60,
            ..w
        };
        assert!(matches!(
            session(&s, &runs, &[late], LATER),
            Verdict::NotClean(_)
        ));
        assert!(window("lunch", 30, 0).is_err() && window("halt", 0, 0).is_err());
    }

    #[test]
    fn the_count_runs_from_the_first_run_and_resets_on_a_bad_session() {
        let days: Vec<Session> = (0..4).map(day).collect();
        let mut runs: Vec<Run> = days.iter().flat_map(full).collect();
        assert_eq!(clean_count(&days, &runs, &[], LATER).unwrap(), (4, None));
        assert_eq!(missed_runs(&days, &runs, &[], LATER).unwrap(), 0);
        // day 1 misses one run: the count restarts after it
        runs.retain(|r| r.at != days[1].open - 1800 + 20);
        let (n, why) = clean_count(&days, &runs, &[], LATER).unwrap();
        assert_eq!(n, 2);
        assert_eq!(why.as_deref(), Some("day1: missed the 09:00 ET run"));
        assert_eq!(missed_runs(&days, &runs, &[], LATER).unwrap(), 1);
        // the session still open stops the count and isn't held against it
        assert_eq!(clean_count(&days, &runs, &[], days[3].close).unwrap().0, 1);
        // nothing recorded is zero, not a pass
        assert_eq!(clean_count(&days, &[], &[], LATER).unwrap(), (0, None));
    }

    #[test]
    fn windows_cannot_stand_in_for_a_day_and_still_need_the_drill_held() {
        let s = day(0);
        assert!(window("halt", WINDOW_MAX_MINUTES + 1, 0).is_err());
        // two windows covering 4 hours: the session doesn't count, whatever ran
        let w = |at| window("dead-man", 120, at).unwrap();
        let both = [w(s.open), w(s.open + 7200)];
        let v = session(&s, &[], &both, LATER);
        assert!(
            matches!(&v, Verdict::NotClean(m) if m.contains("over 120")),
            "{v:?}"
        );
        // inside a halt window the drill must still be held; inside the STOP drill's it may not
        let mut runs = full(&s);
        runs[4].drills = 0;
        let v = session(
            &s,
            &runs,
            &[window("halt", 30, s.open + 600).unwrap()],
            LATER,
        );
        assert!(
            matches!(&v, Verdict::NotClean(m) if m.contains("no drill position")),
            "{v:?}"
        );
        let stop = window("stop", 30, s.open + 600).unwrap();
        assert_eq!(session(&s, &runs, &[stop], LATER), Verdict::Clean);
        // the window's end is exclusive, its start inclusive
        let mut runs = full(&s);
        runs[3].mismatches = vec!["x".into()];
        let ends = window("mismatch", 15, runs[3].at - 900).unwrap(); // ends at the run
        assert!(matches!(
            session(&s, &runs, &[ends], LATER),
            Verdict::NotClean(_)
        ));
        let starts = window("mismatch", 15, runs[3].at).unwrap();
        assert_eq!(session(&s, &runs, &[starts], LATER), Verdict::Clean);
    }

    #[test]
    fn a_record_that_starts_mid_session_skips_that_session() {
        let days: Vec<Session> = (0..3).map(day).collect();
        let mut runs: Vec<Run> = days.iter().flat_map(full).collect();
        runs.retain(|r| r.at >= days[0].open + 3600); // installed at 10:30 on day 0
        assert_eq!(clean_count(&days, &runs, &[], LATER).unwrap(), (2, None));
        assert_eq!(missed_runs(&days, &runs, &[], LATER).unwrap(), 0);
        // a run at exactly slot + 10 min doesn't cover the slot
        let mut late = full(&days[1]);
        late[0].at = days[1].open - 1800 + LATE;
        assert!(matches!(
            session(&days[1], &late, &[], LATER),
            Verdict::NotClean(_)
        ));
        // runs but no calendar is an error, never a zero
        assert!(clean_count(&[], &runs, &[], LATER).is_err());
    }
    #[test]
    fn a_half_day_needs_runs_only_until_its_close() {
        let mut s = day(0);
        s.close = s.open + 210 * 60; // 13:00 ET
        let runs: Vec<Run> = full(&day(0))
            .into_iter()
            .filter(|r| r.at < s.close)
            .collect();
        assert_eq!(runs.len(), 16);
        assert_eq!(session(&s, &runs, &[], LATER), Verdict::Clean);
    }

    #[test]
    fn the_logs_append_and_a_bad_line_is_an_error() {
        let d = std::env::temp_dir().join(format!("bmv-rec-{}", std::process::id()));
        let p = d.join("reconcile/runs.jsonl");
        assert_eq!(read::<Run>(&p).unwrap(), vec![]);
        let r = full(&day(0)).remove(0);
        append(&p, &r).unwrap();
        append(&p, &r).unwrap();
        assert_eq!(read::<Run>(&p).unwrap().len(), 2);
        std::fs::write(&p, "{\"at\": 1}\n").unwrap();
        assert!(read::<Run>(&p).unwrap_err().contains("line 1"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
