//! Research statistics for the P1 gate (DESIGN §13).
//! Trades on the same day are correlated (one market move hits every symbol), so confidence comes
//! from resampling whole days, not single trades. Every variant tried is logged, and the bar rises
//! with the number of trials, so trying many ideas can't manufacture a pass.

use crate::backtest::{DayResult, Summary, summarize};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::ops::Range;
use std::path::Path;

/// Overall chance of a false pass we accept across all trials (DESIGN §13, Bonferroni-style).
pub const FAMILY_ALPHA: f64 = 0.05;
/// Fewer trades than this can't pass, whatever the other numbers say.
pub const MIN_TRADES: usize = 100;

/// R multiples of each day's trades.
pub fn r_by_day(days: &[DayResult]) -> Vec<Vec<f64>> {
    days.iter()
        .map(|d| d.trades.iter().map(|t| t.r_multiple()).collect())
        .collect()
}

/// Lower and upper bounds of the expectancy (mean R per trade), from resampling whole days with
/// replacement. `alpha` is two-sided: 0.05 gives a 95% interval. None if there are no trades.
pub fn bootstrap_expectancy(
    days: &[Vec<f64>],
    iters: usize,
    alpha: f64,
    seed: u64,
) -> Option<(f64, f64)> {
    if days.iter().all(|d| d.is_empty()) || iters == 0 || !(0.0..1.0).contains(&alpha) {
        return None;
    }
    let mut rng = seed.max(1);
    let mut means = Vec::with_capacity(iters);
    for _ in 0..iters {
        let (mut sum, mut n) = (0.0, 0usize);
        for _ in 0..days.len() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let day = &days[(rng % days.len() as u64) as usize];
            sum += day.iter().sum::<f64>();
            n += day.len();
        }
        if n > 0 {
            means.push(sum / n as f64);
        }
    }
    if means.is_empty() {
        return None;
    }
    means.sort_by(f64::total_cmp);
    let at = |q: f64| means[((q * (means.len() - 1) as f64).round() as usize).min(means.len() - 1)];
    Some((at(alpha / 2.0), at(1.0 - alpha / 2.0)))
}

/// Splits `n` days into the research period and a final holdout of the last `holdout` days,
/// which is evaluated once, only for the finalist.
pub fn split_holdout(n: usize, holdout: usize) -> (Range<usize>, Range<usize>) {
    let cut = n.saturating_sub(holdout);
    (0..cut, cut..n)
}

/// Anchored walk-forward over the research period: each fold trains on everything before its test
/// window. Tests windows are `test_len` days, starting after `min_train` days.
pub fn walk_forward(
    n: usize,
    min_train: usize,
    test_len: usize,
) -> Vec<(Range<usize>, Range<usize>)> {
    if test_len == 0 {
        return Vec::new();
    }
    (min_train..n)
        .step_by(test_len)
        .map(|start| (0..start, start..(start + test_len).min(n)))
        .collect()
}

/// One row in the trial log: every variant tried, pass or fail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trial {
    pub when: String,
    pub strategy: String,
    pub params: String,
    pub period: String,
    pub trades: usize,
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub max_drawdown_r: f64,
    pub ci_low: Option<f64>,
}

/// Appends a trial to the JSON-lines log and returns how many trials it now holds.
pub fn record_trial(path: &Path, t: &Trial) -> Result<usize, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let line = serde_json::to_string(t).map_err(|e| e.to_string())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    count_trials(path)
}

/// Trials so far; a line that doesn't parse is an error, because a broken log would undercount.
pub fn count_trials(path: &Path) -> Result<usize, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut n = 0;
    for (i, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        serde_json::from_str::<Trial>(line)
            .map_err(|e| format!("{} line {}: {e}", path.display(), i + 1))?;
        n += 1;
    }
    Ok(n)
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateCheck {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateReport {
    pub checks: Vec<GateCheck>,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.passed)
    }
}

/// The P1 gate (DESIGN §13). `research` is the walk-forward test windows joined together,
/// `baseline` the random-entry control over the same days, `holdout` the final unseen period
/// (None until the finalist is chosen), `trials` how many variants have been tried in total.
pub fn p1_gate(
    research: &[DayResult],
    baseline: &[DayResult],
    holdout: Option<&[DayResult]>,
    trials: usize,
) -> GateReport {
    let s: Summary = summarize(research);
    let b: Summary = summarize(baseline);
    let alpha = FAMILY_ALPHA / trials.max(1) as f64;
    let ci = bootstrap_expectancy(&r_by_day(research), 2_000, alpha, 42);
    let mut checks = vec![
        GateCheck {
            name: "lower confidence bound of expectancy > 0",
            passed: ci.is_some_and(|(lo, _)| lo > 0.0),
            detail: match ci {
                Some((lo, hi)) => format!(
                    "{:.3}R .. {:.3}R at {:.2}% two-sided ({} trials)",
                    lo,
                    hi,
                    100.0 * (1.0 - alpha),
                    trials.max(1)
                ),
                None => "no trades".into(),
            },
        },
        GateCheck {
            name: "beats the random-entry baseline",
            passed: s.trades > 0 && b.trades > 0 && s.expectancy_r > b.expectancy_r,
            detail: format!(
                "{:.3}R vs baseline {:.3}R ({} baseline trades)",
                s.expectancy_r, b.expectancy_r, b.trades
            ),
        },
        GateCheck {
            name: "profit factor > 1.2",
            passed: s.profit_factor > 1.2,
            detail: format!("{:.2}", s.profit_factor),
        },
        GateCheck {
            name: "max drawdown < 10R",
            passed: s.max_drawdown_r < 10.0,
            detail: format!("{:.1}R", s.max_drawdown_r),
        },
        GateCheck {
            name: "enough trades to judge",
            passed: s.trades >= MIN_TRADES,
            detail: format!("{} (need ≥ {MIN_TRADES})", s.trades),
        },
    ];
    match holdout {
        Some(h) => {
            let hs = summarize(h);
            checks.push(GateCheck {
                name: "final holdout expectancy > 0",
                passed: hs.trades > 0 && hs.expectancy_r > 0.0,
                detail: format!("{:.3}R over {} trades", hs.expectancy_r, hs.trades),
            });
        }
        None => checks.push(GateCheck {
            name: "final holdout expectancy > 0",
            passed: false,
            detail: "not evaluated yet (run once, for the finalist only)".into(),
        }),
    }
    GateReport { checks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{ExitReason, Trade};
    use crate::money::{Price, Qty, Usd};

    fn trade(r_mult_cents: i64) -> Trade {
        Trade {
            id: String::new(),
            date: String::new(),
            symbol: "X".into(),
            qty: Qty::new(1).unwrap(),
            entry: Price::from_cents(10_000).unwrap(),
            exit: Price::from_cents(10_000).unwrap(),
            reason: ExitReason::Target,
            r: Usd::from_cents(100),
            pnl: Usd::from_cents(r_mult_cents),
        }
    }

    fn days(pattern: &[&[i64]]) -> Vec<DayResult> {
        pattern
            .iter()
            .map(|d| DayResult {
                trades: d.iter().map(|r| trade(*r)).collect(),
                ..DayResult::default()
            })
            .collect()
    }

    #[test]
    fn bootstrap_brackets_the_mean_and_is_repeatable() {
        let d = r_by_day(&days(&[&[100, -100], &[200], &[-100, 100, 100], &[0]]));
        let (lo, hi) = bootstrap_expectancy(&d, 2_000, 0.05, 7).unwrap();
        let mean = (1.0 - 1.0 + 2.0 - 1.0 + 1.0 + 1.0 + 0.0) / 7.0;
        assert!(lo <= mean && mean <= hi, "{lo} {mean} {hi}");
        assert_eq!(bootstrap_expectancy(&d, 2_000, 0.05, 7), Some((lo, hi)));
    }

    #[test]
    fn a_constant_edge_has_a_tight_interval() {
        let d = vec![vec![0.5]; 50];
        let (lo, hi) = bootstrap_expectancy(&d, 500, 0.05, 1).unwrap();
        assert!((lo - 0.5).abs() < 1e-12 && (hi - 0.5).abs() < 1e-12);
    }

    #[test]
    fn more_trials_widen_the_interval() {
        let d: Vec<Vec<f64>> = (0..60)
            .map(|i| vec![if i % 3 == 0 { -1.0 } else { 1.0 }])
            .collect();
        let (lo1, _) = bootstrap_expectancy(&d, 4_000, 0.05, 3).unwrap();
        let (lo20, _) = bootstrap_expectancy(&d, 4_000, 0.05 / 20.0, 3).unwrap();
        assert!(lo20 < lo1);
    }

    #[test]
    fn bootstrap_needs_trades_and_sane_inputs() {
        assert_eq!(bootstrap_expectancy(&[], 100, 0.05, 1), None);
        assert_eq!(bootstrap_expectancy(&[vec![], vec![]], 100, 0.05, 1), None);
        assert_eq!(bootstrap_expectancy(&[vec![1.0]], 0, 0.05, 1), None);
        assert_eq!(bootstrap_expectancy(&[vec![1.0]], 10, 1.5, 1), None);
    }

    #[test]
    fn holdout_and_walk_forward_splits() {
        assert_eq!(split_holdout(100, 20), (0..80, 80..100));
        assert_eq!(split_holdout(10, 20), (0..0, 0..10));
        assert_eq!(walk_forward(10, 4, 3), vec![(0..4, 4..7), (0..7, 7..10)]);
        assert_eq!(
            walk_forward(11, 4, 3),
            vec![(0..4, 4..7), (0..7, 7..10), (0..10, 10..11)]
        );
        assert!(walk_forward(10, 10, 3).is_empty());
        assert!(walk_forward(10, 2, 0).is_empty());
        // test windows never overlap and never include training days
        for (train, test) in walk_forward(250, 120, 21) {
            assert!(train.end == test.start && !test.is_empty());
        }
    }

    #[test]
    fn trial_log_appends_and_counts_and_refuses_corruption() {
        let dir = std::env::temp_dir().join(format!("bmv-trials-{}", std::process::id()));
        let path = dir.join("trials.jsonl");
        let _ = std::fs::remove_file(&path);
        let t = Trial {
            when: "now".into(),
            strategy: "orb".into(),
            params: "default".into(),
            period: "2020..2024".into(),
            trades: 10,
            expectancy_r: 0.1,
            profit_factor: 1.3,
            max_drawdown_r: 4.0,
            ci_low: Some(-0.05),
        };
        assert_eq!(count_trials(&path), Ok(0));
        assert_eq!(record_trial(&path, &t), Ok(1));
        assert_eq!(record_trial(&path, &t), Ok(2));
        std::fs::write(&path, "{not json}\n").unwrap();
        assert!(count_trials(&path).is_err());
    }

    #[test]
    fn the_gate_passes_only_when_every_check_does() {
        // a steady +0.5R edge over 60 days, baseline at −0.1R, holdout positive
        let good = days(&[&[150i64, -100][..]; 60]); // mean +0.25R per trade
        let base = days(&[&[-100i64, 80][..]; 60]); // mean −0.1R
        let hold = days(&[&[150i64, -100][..]; 10]);
        let r = p1_gate(&good, &base, Some(&hold), 1);
        assert!(r.passed(), "{r:#?}");
        // without the holdout it can't pass yet
        assert!(!p1_gate(&good, &base, None, 1).passed());
        // worse than the baseline fails
        assert!(!p1_gate(&base, &good, Some(&hold), 1).passed());
        // no trades fails
        assert!(!p1_gate(&[], &base, Some(&hold), 1).passed());
    }

    #[test]
    fn a_noisy_edge_passes_alone_but_not_after_many_trials() {
        // +1R / −1R coin with a slight tilt
        let pattern: Vec<&[i64]> = (0..120)
            .map(|i| {
                if i % 5 < 3 {
                    &[100i64][..]
                } else {
                    &[-100i64][..]
                }
            })
            .collect();
        let research = days(&pattern);
        let base = days(&[&[-100i64, 80][..]; 60]);
        let hold = days(&[&[100i64][..]; 5]);
        let ci = |trials| p1_gate(&research, &base, Some(&hold), trials).checks[0].passed;
        assert!(ci(1), "one trial: the interval clears zero");
        assert!(
            !ci(10_000),
            "after 10,000 trials the same evidence isn't enough"
        );
    }
}
