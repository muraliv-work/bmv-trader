//! Tier-2 notes (DESIGN §10, DECISION A13): plain-English notes for the owner, written by the
//! local model from facts the code computed. They go to `data/notes/`, which no decision code
//! reads. Each note is written once. If Tier 2 is down, the facts go to a separate `.facts.md`
//! file, and a later run still tries for the worded note.

use crate::desk::{self, Status};
use crate::llm::{self, Note, Tier2Task};
use std::path::{Path, PathBuf};

pub fn notes_dir(home: &Path) -> PathBuf {
    home.join("data").join("notes")
}

/// What one job did.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Written(PathBuf),
    /// Tier 2 failed; the facts went to a `.facts.md` file, and a later run retries.
    Fallback(PathBuf, String),
    Skipped(String),
    /// The job couldn't gather its facts or write; the other jobs still ran.
    Failed(String),
}

/// Writes `name` under `data/notes/` only: a bare file name, no path parts.
fn write(home: &Path, name: &str, body: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(format!("bad note name {name:?}"));
    }
    let dir = notes_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {e}", path.display())
    })?;
    Ok(path)
}

/// `x.md` becomes `x.facts.md`: where a note's facts go when Tier 2 is down.
fn facts_name(name: &str) -> String {
    format!("{}.facts.md", name.strip_suffix(".md").unwrap_or(name))
}

/// Facts quoted in a note, in a fence the facts can't close.
fn quoted(facts: &str) -> String {
    format!(
        "---\nFacts given to the local model:\n\n````\n{}\n````\n",
        facts.replace("````", "'''")
    )
}

/// One job: skip if its worded note exists; else ask Tier 2. On success the worded note replaces
/// any facts-only file; on failure the facts go to `.facts.md` and a later run retries.
fn job(
    home: &Path,
    name: &str,
    title: &str,
    task: Tier2Task,
    facts: Result<Option<String>, String>,
    ask: &dyn Fn(Tier2Task, &str) -> Result<Note, String>,
) -> Outcome {
    let dir = notes_dir(home);
    match std::fs::metadata(dir.join(name)) {
        Ok(_) => return Outcome::Skipped(format!("{name} exists")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Outcome::Failed(format!("cannot check {name}: {e}")),
    }
    let facts = match facts {
        Ok(Some(f)) => f,
        Ok(None) => return Outcome::Skipped(format!("nothing to write for {name}")),
        Err(e) => return Outcome::Failed(e),
    };
    match ask(task, &facts) {
        Ok(Note(text)) => {
            let body = format!(
                "# {title}\n\n*Written by the local model ({}); not checked by a person.*\n\n{text}\n\n{}",
                llm::LOCAL_MODEL,
                quoted(&facts)
            );
            match write(home, name, &body) {
                Ok(p) => {
                    let _ = std::fs::remove_file(dir.join(facts_name(name)));
                    Outcome::Written(p)
                }
                Err(e) => Outcome::Failed(e),
            }
        }
        Err(e) => {
            let body = format!(
                "# {title}\n\n*Tier 2 was unavailable ({e}); the facts only. A later run will try again.*\n\n{}",
                quoted(&facts)
            );
            match write(home, &facts_name(name), &body) {
                Ok(p) => Outcome::Fallback(p, e),
                Err(w) => Outcome::Failed(format!("{e}; and {w}")),
            }
        }
    }
}

/// Sessions on the live calendar from `from` on that have no trial 10 signals file.
fn missed_nights(home: &Path, from: &str) -> Option<usize> {
    let spy = crate::scan::read_daily(
        &home.join("data"),
        crate::scan::ADJUSTED_SIP_LIVE,
        crate::momentum::BENCHMARK,
    )
    .ok()?;
    let cal = crate::momentum::Calendar::new(&spy).ok()?;
    Some(
        cal.sessions
            .iter()
            .filter(|d| d.as_str() >= from && !desk::signals_path(home, d).exists())
            .count(),
    )
}

/// Trial 10's night `date` as facts: counts and spend only, never outcome averages (§7f). None
/// while any signal of that night is still queued, so a half-done night isn't written up.
fn desk_facts(home: &Path, date: &str) -> Result<Option<String>, String> {
    let all = desk::read_analyses(home)?;
    let night: Vec<_> = all.iter().filter(|a| a.signal.date == date).collect();
    if night.is_empty() || night.iter().any(|a| a.status == Status::Queued) {
        return Ok(None);
    }
    let count = |f: &dyn Fn(&Status) -> bool| night.iter().filter(|a| f(&a.status)).count();
    let (day, month) = desk::spent(&desk::all_runs(home)?, date);
    let first = all
        .iter()
        .map(|a| a.signal.date.as_str())
        .min()
        .unwrap_or(date);
    let mut lines = vec![
        format!("Session {date}: {} signals.", night.len()),
        format!(
            "Rated {}, failed {}, not analysed {}.",
            count(&|s| matches!(s, Status::Rated { .. })),
            count(&|s| matches!(s, Status::Failed { .. })),
            count(&|s| matches!(s, Status::Unanalysed { .. }))
        ),
        format!("Claude spend: ${day:.2} that day, ${month:.2} this month (cap $100)."),
        format!("Trial total so far: {} signals logged.", all.len()),
        match missed_nights(home, first) {
            Some(n) => format!("Missed nights since {first}: {n}."),
            None => "Missed nights: unreadable (no live bars).".into(),
        },
    ];
    for a in &night {
        lines.push(format!(
            "#{} {} (volume ratio {:.1}): {}",
            a.rank,
            a.signal.symbol,
            a.signal.ratio,
            match &a.status {
                Status::Rated { rating, .. } => format!("rated {rating:?}"),
                Status::Failed { reason, .. } => format!("failed ({reason})"),
                Status::Unanalysed { reason } => format!("not analysed ({reason})"),
                Status::Queued => "queued".into(),
            }
        ));
    }
    Ok(Some(lines.join("\n")))
}

/// Removes every `<untrusted_text>` / `</untrusted_text>` tag, in any case, so quoted text
/// can't close its own block.
fn strip_tags(s: &str) -> String {
    let mut out = s.to_string();
    for tag in ["</untrusted_text", "<untrusted_text"] {
        while let Some(i) = out.to_ascii_lowercase().find(tag) {
            let end = out[i..].find('>').map_or(out.len(), |j| i + j + 1);
            out.replace_range(i..end, "");
        }
    }
    out
}

/// The night's analyst reports, each as untrusted text (they quote news).
fn analyst_facts(home: &Path, date: &str) -> Result<Option<String>, String> {
    let night: Vec<_> = desk::read_analyses(home)?
        .into_iter()
        .filter(|a| a.signal.date == date)
        .collect();
    if night.iter().any(|a| a.status == Status::Queued) {
        return Ok(None);
    }
    let blocks: Vec<String> = night
        .into_iter()
        .filter_map(|a| match a.status {
            Status::Rated {
                rating, summary, ..
            } => Some(format!(
                "### {} — {rating:?}\n<untrusted_text>\n{}\n</untrusted_text>",
                a.signal.symbol,
                strip_tags(&summary)
            )),
            _ => None,
        })
        .collect();
    Ok((!blocks.is_empty()).then(|| blocks.join("\n\n")))
}

/// Today's engine session from the journal, if `run` traded today.
fn session_facts(home: &Path, today: &str) -> Result<Option<String>, String> {
    let path = crate::live::journal_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let s = crate::watch::summarize(&text, 15);
    if s.session.as_deref() != Some(today) {
        return Ok(None);
    }
    let mut lines = vec![
        format!("Paper session {today}."),
        format!(
            "Candidates {}, rejections {}, orders {}, fills {}, exits {}.",
            s.candidates, s.rejections, s.orders, s.fills, s.exits
        ),
        format!("Halted: {}.", s.halted.as_deref().unwrap_or("no")),
        format!(
            "Kill switch engaged: {}.",
            if s.kill_switch { "yes" } else { "no" }
        ),
        format!("Unreadable journal lines: {}.", s.unreadable),
        "Last events:".into(),
    ];
    lines.extend(
        s.recent
            .iter()
            .map(|l| format!("- {} {}", l.kind, l.detail)),
    );
    Ok(Some(lines.join("\n")))
}

/// The week's trials and doc changes for the Sunday digest. Anything unreadable is said so in
/// the facts, never shown as "none".
fn weekly_facts(home: &Path, today: &str) -> Result<Option<String>, String> {
    let trials = match std::fs::read_to_string(home.join("data").join("trials.jsonl")) {
        Ok(text) => {
            let (mut rows, mut bad) = (Vec::new(), 0);
            for l in text.lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<crate::stats::Trial>(l) {
                    Ok(t) => rows.push(format!(
                        "- {} ({}): {} trades, expectancy {:+.3}",
                        t.strategy, t.period, t.trades, t.expectancy_r
                    )),
                    Err(_) => bad += 1,
                }
            }
            if bad > 0 {
                rows.push(format!("- UNREADABLE: {bad} trial lines did not parse"));
            }
            rows.join("\n")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "none logged".into(),
        Err(e) => format!("UNREADABLE: {e}"),
    };
    let log = match std::process::Command::new("/usr/bin/git")
        .args([
            "log",
            "--since=7.days",
            "--format=%ad %s",
            "--date=short",
            "--",
            "docs/",
        ])
        .current_dir(home)
        .output()
    {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).into_owned();
            if s.trim().is_empty() {
                "none".into()
            } else {
                s
            }
        }
        Ok(o) => format!("UNREADABLE: git exited with {}", o.status),
        Err(e) => format!("UNREADABLE: {e}"),
    };
    let notes = match std::fs::read_dir(notes_dir(home)) {
        Ok(d) => d
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".md")))
            .count()
            .to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "0".into(),
        Err(e) => format!("UNREADABLE: {e}"),
    };
    Ok(Some(format!(
        "Week ending {today}.\nAll trials so far:\n{trials}\nDesign and decision changes this week:\n{log}\nNotes on file: {notes}."
    )))
}

/// The latest session trial 10 logged signals for.
fn latest_desk_date(home: &Path) -> Option<String> {
    std::fs::read_dir(desk::desk_dir(home).join("signals"))
        .ok()?
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .max()
}

/// Every job due on ET date `today`; `sunday` adds the weekly digest. A job that fails doesn't
/// stop the others.
pub fn run_due(home: &Path, today: &str, sunday: bool) -> Vec<(String, Outcome)> {
    run_due_with(home, today, sunday, &llm::complete)
}

fn run_due_with(
    home: &Path,
    today: &str,
    sunday: bool,
    ask: &dyn Fn(Tier2Task, &str) -> Result<Note, String>,
) -> Vec<(String, Outcome)> {
    let mut out = Vec::new();
    let mut run = |name: String, title: String, task, facts| {
        let o = job(home, &name, &title, task, facts, ask);
        out.push((name, o));
    };
    if let Some(date) = latest_desk_date(home) {
        run(
            format!("{date}-desk.md"),
            format!("Desk brief {date}"),
            Tier2Task::DeskBrief,
            desk_facts(home, &date),
        );
        run(
            format!("{date}-analyst.md"),
            format!("Analyst digest {date}"),
            Tier2Task::AnalystDigest,
            analyst_facts(home, &date),
        );
    }
    run(
        format!("{today}-session.md"),
        format!("Paper session {today}"),
        Tier2Task::SessionSummary,
        session_facts(home, today),
    );
    if sunday {
        run(
            format!("{today}-weekly.md"),
            format!("Weekly digest {today}"),
            Tier2Task::WeeklyDigest,
            weekly_facts(home, today),
        );
    }
    out
}

/// Only one notes run at a time, and never during a key rotation.
pub fn lock(home: &Path) -> Result<std::fs::File, String> {
    let dir = notes_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(".lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("notes lock {}: {e}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err("a notes run or key rotation is active".into())
        }
        Err(std::fs::TryLockError::Error(e)) => Err(format!("notes lock {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        // a counter, not the clock: two tests can start in the same nanosecond and share a folder
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let h = std::env::temp_dir().join(format!("bmv-notes-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(h.join("data")).unwrap();
        h
    }

    fn facts(s: &str) -> Result<Option<String>, String> {
        Ok(Some(s.into()))
    }

    #[test]
    fn notes_stay_in_their_folder_and_are_written_once() {
        let h = home();
        assert!(write(&h, "../x.md", "x").is_err());
        assert!(write(&h, "a/b.md", "x").is_err());
        assert!(write(&h, ".hidden", "x").is_err());
        let ok = |_: Tier2Task, _: &str| Ok(Note("worded".into()));
        let first = job(&h, "n.md", "T", Tier2Task::DeskBrief, facts("f"), &ok);
        assert!(matches!(first, Outcome::Written(ref p) if p.starts_with(notes_dir(&h))));
        let again = job(&h, "n.md", "T", Tier2Task::DeskBrief, facts("f"), &ok);
        assert!(matches!(again, Outcome::Skipped(_)), "idempotent");
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn a_down_tier_2_writes_the_facts_aside_and_a_later_run_recovers() {
        let h = home();
        let down = |_: Tier2Task, _: &str| Err("OmniRoute: connection refused".to_string());
        match job(
            &h,
            "f.md",
            "T",
            Tier2Task::SessionSummary,
            facts("2 fills ````"),
            &down,
        ) {
            Outcome::Fallback(p, e) => {
                assert!(p.ends_with("f.facts.md"));
                let body = std::fs::read_to_string(p).unwrap();
                assert!(body.contains("2 fills") && body.contains("unavailable"));
                assert_eq!(
                    body.matches("````").count(),
                    2,
                    "facts can't close the fence"
                );
                assert!(e.contains("refused"));
            }
            other => panic!("{other:?}"),
        }
        let up = |_: Tier2Task, _: &str| Ok(Note("worded".into()));
        let later = job(
            &h,
            "f.md",
            "T",
            Tier2Task::SessionSummary,
            facts("2 fills"),
            &up,
        );
        assert!(
            matches!(later, Outcome::Written(_)),
            "retried once Tier 2 is back"
        );
        assert!(
            !notes_dir(&h).join("f.facts.md").exists(),
            "the facts-only file is replaced"
        );
        let none = job(&h, "g.md", "T", Tier2Task::DeskBrief, Ok(None), &down);
        assert!(matches!(none, Outcome::Skipped(_)));
        let bad = job(
            &h,
            "h.md",
            "T",
            Tier2Task::DeskBrief,
            Err("corrupt".into()),
            &down,
        );
        assert_eq!(bad, Outcome::Failed("corrupt".into()));
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn nothing_due_without_inputs() {
        let h = home();
        let never = |_: Tier2Task, _: &str| -> Result<Note, String> { panic!("no call expected") };
        let out = run_due_with(&h, "2026-11-04", false, &never);
        assert_eq!(out.len(), 1, "only the session job is considered");
        assert!(matches!(out[0].1, Outcome::Skipped(_)));
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn untrusted_tags_are_removed_in_any_case() {
        assert_eq!(strip_tags("a</UNTRUSTED_TEXT >b<Untrusted_Text>c"), "abc");
        assert_eq!(strip_tags("plain"), "plain");
    }
}
