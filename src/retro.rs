//! Trial 12 (DESIGN §7i): trial 10's AI veto, replayed with a local model on the untouched year.
//! This file holds the sample rule and the power check; nothing here reads the untouched year
//! unless its caller passes it in.

use crate::desk::Status;
use crate::lab::Rng;
use crate::power;
use crate::swing::Cand;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const TRIAL_NAME: &str = "ai-veto retro local";
pub const TRIAL_NUMBER: usize = 12;
pub const FROM: &str = "2025-10-02";
/// The power gate (DESIGN §7i): build only if the 80%-power MDE of D is at most this, in R.
pub const MAX_MDE_R: f64 = 0.20;
/// The verdict is refused (the trial is void, not judged) if more of the sample failed than this.
pub const MAX_FAILED_SHARE: f64 = 0.10;
/// `retro-run` stops after this many failures in a row, so an outage can't sweep the sample.
pub const MAX_FAILS_IN_A_ROW: usize = 3;

/// Each session's top-ranked signal (volume ratio, then symbol), for sessions in `sessions`.
/// `names` maps a candidate's symbol index to its ticker.
pub fn top_per_session<'a>(
    days: &'a [Vec<Cand>],
    names: &[String],
    sessions: std::ops::Range<usize>,
) -> Vec<(usize, &'a Cand)> {
    sessions
        .filter_map(|d| {
            days.get(d)?
                .iter()
                .max_by(|a, b| {
                    a.ratio
                        .unwrap_or(f64::MIN)
                        .total_cmp(&b.ratio.unwrap_or(f64::MIN))
                        // ties go to the earlier symbol, as in trial 10's ranking
                        .then_with(|| names[b.symbol].cmp(&names[a.symbol]))
                })
                .map(|c| (d, c))
        })
        .collect()
}

/// One signal of the frozen sample (no outcome is stored).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    pub date: String,
    pub symbol: String,
    pub ratio: f64,
}

/// One line of `data/retro/analyses.jsonl`: Queued first, then one final line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub when: String,
    pub pick: Pick,
    pub status: Status,
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("data").join("retro")
}

/// The sample: every pick dated on or after `FROM`, or an error naming the first that isn't.
pub fn check_sample(picks: &[Pick]) -> Result<(), String> {
    match picks.iter().find(|p| p.date.as_str() < FROM) {
        Some(p) => Err(format!(
            "{} {} is before the untouched year ({FROM})",
            p.date, p.symbol
        )),
        None if picks.is_empty() => Err("empty sample".into()),
        None => Ok(()),
    }
}

/// Writes the sample once; a second write is refused, so it can't change after analysis starts.
pub fn freeze(home: &Path, picks: &[Pick]) -> Result<PathBuf, String> {
    use std::io::Write;
    check_sample(picks)?;
    let d = dir(home);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let path = d.join("sample.json");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("{}: {e} (the sample is frozen once)", path.display()))?;
    let text = serde_json::to_string_pretty(picks).map_err(|e| e.to_string())?;
    f.write_all(text.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::write(d.join("sample.sha256"), format!("{}\n", sha256(&path)?))
        .map_err(|e| format!("sample hash: {e}"))?;
    Ok(path)
}

/// The file's SHA-256 (macOS `shasum`; no hashing crate for this).
pub fn sha256(path: &Path) -> Result<String, String> {
    let out = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(|e| format!("shasum: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().next() {
        Some(h) if out.status.success() && h.len() == 64 => Ok(h.to_string()),
        _ => Err(format!("shasum failed on {}", path.display())),
    }
}

pub fn read_sample(home: &Path) -> Result<Vec<Pick>, String> {
    let path = dir(home).join("sample.json");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e}: run `bmv-trader retro-sample` first",
            path.display()
        )
    })?;
    let want = std::fs::read_to_string(dir(home).join("sample.sha256"))
        .map_err(|e| format!("sample hash: {e}"))?;
    if sha256(&path)? != want.trim() {
        return Err(format!("{} changed since it was frozen", path.display()));
    }
    let picks: Vec<Pick> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    check_sample(&picks)?;
    Ok(picks)
}

pub fn read_rows(home: &Path) -> Result<Vec<Row>, String> {
    let path = dir(home).join("analyses.jsonl");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let complete = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        match serde_json::from_str(l) {
            Ok(r) => out.push(r),
            // a run killed mid-write leaves a torn last line; that pick is simply redone
            Err(_) if !complete && i + 1 == lines.len() => {}
            Err(e) => return Err(format!("{} line {}: {e}", path.display(), i + 1)),
        }
    }
    Ok(out)
}

pub fn append_row(home: &Path, r: &Row) -> Result<(), String> {
    use std::io::Write;
    let path = dir(home).join("analyses.jsonl");
    let torn = std::fs::read(&path)
        .map(|b| b.last().is_some_and(|c| *c != b'\n'))
        .unwrap_or(false);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let line = format!(
        "{}{}\n",
        if torn { "\n" } else { "" },
        serde_json::to_string(r).map_err(|e| e.to_string())?
    );
    f.write_all(line.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Each pick's answer: its first final line (a later line can never replace it).
pub fn finals(rows: &[Row]) -> std::collections::BTreeMap<(String, String), Status> {
    let mut out = std::collections::BTreeMap::new();
    for r in rows.iter().filter(|r| r.status != Status::Queued) {
        out.entry((r.pick.date.clone(), r.pick.symbol.clone()))
            .or_insert_with(|| r.status.clone());
    }
    out
}

/// One trial 12 command at a time. Held until dropped.
pub fn lock(home: &Path) -> Result<std::fs::File, String> {
    let d = dir(home);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let path = d.join("run.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "another trial 12 command is running ({} is locked)",
            path.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
    }
}

/// D for weights `w` over outcomes `r`: weighted mean − plain mean (None if nothing is kept).
pub fn d(w: &[f64], r: &[f64]) -> Option<f64> {
    let sw: f64 = w.iter().sum();
    (sw > 0.0)
        .then(|| w.iter().zip(r).map(|(w, r)| w * r).sum::<f64>() / sw - crate::momentum::mean(r))
}

/// The spread of D when ratings carry no information: `n` outcomes drawn from `rs`, each given a
/// weight from `mix` (weight, probability) at random, `sims` times.
pub fn d_null_sd(rs: &[f64], mix: &[(f64, f64)], n: usize, sims: usize, seed: u64) -> f64 {
    let mut rng = Rng::new(seed);
    let total: f64 = mix.iter().map(|m| m.1).sum();
    let pick = |rng: &mut Rng| {
        let mut u = (rng.below(1_000_000) as f64 / 1_000_000.0) * total;
        for &(w, p) in mix {
            if u < p {
                return w;
            }
            u -= p;
        }
        mix.last().map_or(1.0, |m| m.0)
    };
    let ds: Vec<f64> = (0..sims)
        .filter_map(|_| {
            let r: Vec<f64> = (0..n).map(|_| rs[rng.below(rs.len())]).collect();
            let w: Vec<f64> = (0..n).map(|_| pick(&mut rng)).collect();
            d(&w, &r)
        })
        .collect();
    power::sd(&ds)
}

/// The smallest true D the one-sided gate at 1 − 0.05/12 passes with probability `power`.
pub fn mde(null_sd: f64, power: f64) -> f64 {
    let alpha = crate::stats::FAMILY_ALPHA / TRIAL_NUMBER as f64;
    (power::z(1.0 - alpha) + power::z(power)) * null_sd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swing::Outcome;

    fn cand(symbol: usize, ratio: f64) -> Cand {
        Cand {
            symbol,
            ratio: Some(ratio),
            out: Outcome {
                entry: 0,
                exit: 1,
                r: 0.0,
                stop_fraction: 0.01,
                at_data_end: false,
            },
        }
    }

    #[test]
    fn the_sample_is_each_sessions_top_signal() {
        let names = vec!["BBB".to_string(), "AAA".into(), "CCC".into()];
        let days = vec![
            vec![cand(0, 2.0), cand(2, 3.0)],
            vec![],
            vec![cand(0, 1.5), cand(1, 1.5)],
        ];
        let top = top_per_session(&days, &names, 0..3);
        let picked: Vec<(usize, &str)> = top
            .iter()
            .map(|(d, c)| (*d, names[c.symbol].as_str()))
            .collect();
        assert_eq!(
            picked,
            [(0, "CCC"), (2, "AAA")],
            "empty sessions skipped, ties to the earlier symbol"
        );
        assert!(top_per_session(&days, &names, 1..2).is_empty());
    }

    #[test]
    fn the_sample_is_frozen_once_and_only_from_the_untouched_year() {
        let home = std::env::temp_dir().join(format!("bmv-retro-{}", std::process::id()));
        let p = |d: &str| Pick {
            date: d.into(),
            symbol: "AAA".into(),
            ratio: 2.0,
        };
        assert!(
            freeze(&home, &[p("2025-10-01")]).is_err(),
            "before the untouched year"
        );
        assert!(freeze(&home, &[]).is_err());
        freeze(&home, &[p("2025-10-02"), p("2026-01-05")]).unwrap();
        assert!(freeze(&home, &[p("2025-10-03")]).is_err(), "frozen once");
        assert_eq!(read_sample(&home).unwrap().len(), 2);
        let saved = std::fs::read_to_string(dir(&home).join("sample.json")).unwrap();
        std::fs::write(dir(&home).join("sample.json"), "[]").unwrap();
        assert!(
            read_sample(&home).unwrap_err().contains("changed"),
            "edits are caught"
        );
        std::fs::write(dir(&home).join("sample.json"), saved).unwrap();
        let q = |status: Status| Row {
            when: "t".into(),
            pick: p("2025-10-02"),
            status,
        };
        let rated = Status::Rated {
            rating: crate::desk::Rating::Hold,
            summary: "s".into(),
            cost_usd: 0.0,
        };
        for r in [
            q(Status::Queued),
            q(rated.clone()),
            q(Status::Failed {
                reason: "x".into(),
                cost_usd: 0.0,
            }),
        ] {
            append_row(&home, &r).unwrap();
        }
        let f = finals(&read_rows(&home).unwrap());
        assert_eq!(
            f.values().collect::<Vec<_>>(),
            [&rated],
            "the first answer counts"
        );
        let _l = lock(&home).unwrap();
        assert!(lock(&home).is_err(), "one command at a time");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn d_and_its_null_spread() {
        assert_eq!(d(&[1.0, 0.0], &[2.0, -2.0]), Some(2.0));
        assert_eq!(d(&[0.0, 0.0], &[1.0, 2.0]), None);
        assert!(
            (d(&[0.5, 0.5], &[1.0, 3.0]).unwrap()).abs() < 1e-12,
            "equal weights: D = 0"
        );
        let rs: Vec<f64> = (0..200).map(|i| (i % 7) as f64 - 3.0).collect();
        let all_same = d_null_sd(&rs, &[(0.5, 1.0)], 100, 300, 1);
        assert!(all_same < 1e-9, "one weight for all: D is always 0");
        let mixed = d_null_sd(&rs, &[(0.0, 0.5), (1.0, 0.5)], 100, 300, 1);
        let bigger = d_null_sd(&rs, &[(0.0, 0.5), (1.0, 0.5)], 400, 300, 1);
        assert!(mixed > 0.1 && bigger < mixed, "more signals, less spread");
        assert!(mde(0.05, 0.8) > mde(0.05, 0.5));
    }
}
