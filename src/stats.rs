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
/// Fewer research trades than this can't pass, whatever the other numbers say.
pub const MIN_TRADES: usize = 100;
pub const MIN_HOLDOUT_TRADES: usize = 30;
/// Share of walk-forward test windows that must be profitable on their own.
pub const MIN_POSITIVE_FOLDS: f64 = 0.6;
/// An interval needs at least this many resamples beyond each bound to mean anything.
const TAIL_DRAWS: f64 = 20.0;
/// The most resamples the gate will run; past this the strictness level can't be met and it fails.
pub const MAX_RESAMPLES: usize = 400_000;

/// R multiples of each day's trades.
pub fn r_by_day(days: &[DayResult]) -> Vec<Vec<f64>> {
    days.iter()
        .map(|d| d.trades.iter().map(|t| t.r_multiple()).collect())
        .collect()
}

/// Resamples a two-sided `alpha` needs so each tail holds at least 20 draws.
pub fn resamples_for(alpha: f64) -> usize {
    ((2.0 * TAIL_DRAWS / alpha).ceil() as usize).max(2_000)
}

/// Lower and upper bounds of the expectancy (mean R per trade), from resampling whole days with
/// replacement. `alpha` is two-sided: 0.05 gives a 95% interval. None if there are no trades, or if
/// `iters` is too small for `alpha` (each tail must hold 20 draws), so a too-strict level fails
/// rather than quietly returning the minimum of the draws.
pub fn bootstrap_expectancy(
    days: &[Vec<f64>],
    iters: usize,
    alpha: f64,
    seed: u64,
) -> Option<(f64, f64)> {
    let tails_ok = alpha / 2.0 * iters as f64 >= TAIL_DRAWS;
    if days.iter().all(|d| d.is_empty()) || !(alpha > 0.0 && alpha < 1.0) || !tails_ok {
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
        // a resample of only no-trade days has no expectancy; skipping it slightly favours
        // sparse data, which MIN_TRADES guards against
        if n > 0 {
            means.push(sum / n as f64);
        }
    }
    if means.is_empty() {
        return None;
    }
    means.sort_by(f64::total_cmp);
    let last = means.len() - 1;
    // round outward so rounding never narrows the interval
    let lo = ((alpha / 2.0) * last as f64).floor() as usize;
    let hi = ((1.0 - alpha / 2.0) * last as f64).ceil() as usize;
    Some((means[lo.min(last)], means[hi.min(last)]))
}

/// Splits `n` days into the research period and a final holdout of the last `holdout` days,
/// which is evaluated once, only for the finalist.
pub fn split_holdout(n: usize, holdout: usize) -> (Range<usize>, Range<usize>) {
    let cut = n.saturating_sub(holdout);
    (0..cut, cut..n)
}

/// Anchored walk-forward over the research period `0..n` (pass `n` = the research end, never the
/// holdout): each fold trains on everything before its test window of `test_len` days.
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

/// One row in the trial log: every variant tried, pass or fail, including holdout runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trial {
    pub when: String,
    pub strategy: String,
    pub params: String,
    pub period: String,
    pub trades: usize,
    pub expectancy_r: f64,
    /// None when there were no losing trades (an unbounded profit factor).
    pub profit_factor: Option<f64>,
    pub max_drawdown_r: f64,
    pub ci_low: Option<f64>,
}

impl Trial {
    pub fn from_summary(
        when: &str,
        strategy: &str,
        params: &str,
        period: &str,
        s: &Summary,
        ci_low: Option<f64>,
    ) -> Self {
        Trial {
            when: when.into(),
            strategy: strategy.into(),
            params: params.into(),
            period: period.into(),
            trades: s.trades,
            expectancy_r: s.expectancy_r,
            profit_factor: s.profit_factor.is_finite().then_some(s.profit_factor),
            max_drawdown_r: s.max_drawdown_r,
            ci_low,
        }
    }
}

/// Appends a trial to the JSON-lines log and returns how many trials it now holds.
/// Non-finite numbers are refused: JSON can't hold them, and a line that can't be read back
/// would break the count for every later trial.
pub fn record_trial(path: &Path, t: &Trial) -> Result<usize, String> {
    let nums = [
        Some(t.expectancy_r),
        t.profit_factor,
        Some(t.max_drawdown_r),
        t.ci_low,
    ];
    if nums.iter().flatten().any(|x| !x.is_finite()) {
        return Err(format!("trial has a non-finite number: {t:?}"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let line = serde_json::to_string(t).map_err(|e| e.to_string())? + "\n";
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    f.write_all(line.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| {
            format!(
                "cannot write {} (the trial may be partly recorded): {e}",
                path.display()
            )
        })?;
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

/// Every check of the P1 gate; it passes only if all of them do.
#[derive(Clone, Debug, PartialEq)]
pub struct GateReport {
    pub checks: Vec<GateCheck>,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.passed)
    }
}

/// The P1 gate (DESIGN §13).
/// - `folds`: the walk-forward test windows in date order, each a list of days
/// - `baseline`: the random-entry control over the same days, pooled over several seeds
/// - `holdout`: the final unseen period (None until the finalist is chosen)
/// - `trial_log`: the trial log; the strictness level comes from how many trials it holds, so
///   record this run before calling
pub fn p1_gate(
    folds: &[Vec<DayResult>],
    baseline: &[DayResult],
    holdout: Option<&[DayResult]>,
    trial_log: &Path,
) -> Result<GateReport, String> {
    let trials = count_trials(trial_log)?;
    if trials == 0 {
        return Err(format!(
            "no trials logged in {}: record this run before gating it",
            trial_log.display()
        ));
    }
    let research: Vec<DayResult> = folds.iter().flatten().cloned().collect();
    let s = summarize(&research);
    let b = summarize(baseline);
    let alpha = FAMILY_ALPHA / trials as f64;
    let iters = resamples_for(alpha);
    let ci = if iters <= MAX_RESAMPLES {
        bootstrap_expectancy(&r_by_day(&research), iters, alpha, 42)
    } else {
        None
    };
    let ci_detail = match ci {
        Some((lo, hi)) => format!(
            "{lo:.3}R .. {hi:.3}R at {:.4}% two-sided ({trials} trials)",
            100.0 * (1.0 - alpha)
        ),
        None if iters > MAX_RESAMPLES => {
            format!("{trials} trials need {iters} resamples (max {MAX_RESAMPLES}): too many trials")
        }
        None => "no trades".into(),
    };
    let positive_folds = folds
        .iter()
        .filter(|f| summarize(f).expectancy_r > 0.0)
        .count();
    let mut checks = vec![
        GateCheck {
            name: "lower confidence bound of expectancy > 0",
            passed: ci.is_some_and(|(lo, _)| lo > 0.0),
            detail: ci_detail.clone(),
        },
        GateCheck {
            name: "lower confidence bound beats the random-entry baseline",
            passed: b.trades > 0 && ci.is_some_and(|(lo, _)| lo > b.expectancy_r),
            detail: format!(
                "baseline {:.3}R over {} trades; research {}",
                b.expectancy_r, b.trades, ci_detail
            ),
        },
        GateCheck {
            name: "most walk-forward windows profitable",
            passed: !folds.is_empty()
                && positive_folds as f64 >= MIN_POSITIVE_FOLDS * folds.len() as f64,
            detail: format!(
                "{positive_folds} of {} (need ≥ {:.0}%)",
                folds.len(),
                100.0 * MIN_POSITIVE_FOLDS
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
    checks.push(match holdout {
        Some(h) => {
            let hs = summarize(h);
            GateCheck {
                name: "final holdout expectancy > 0",
                passed: hs.trades >= MIN_HOLDOUT_TRADES && hs.expectancy_r > 0.0,
                detail: format!(
                    "{:.3}R over {} trades (need ≥ {MIN_HOLDOUT_TRADES})",
                    hs.expectancy_r, hs.trades
                ),
            }
        }
        None => GateCheck {
            name: "final holdout expectancy > 0",
            passed: false,
            detail: "not evaluated yet (run once, for the finalist only, and log it as a trial)"
                .into(),
        },
    });
    Ok(GateReport { checks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{ExitReason, Trade};
    use crate::money::{Price, Qty, Usd};
    use std::path::PathBuf;

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

    /// A trial log holding `n` trials.
    fn log_with(name: &str, n: usize) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("bmv-trials-{}-{name}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let t = Trial::from_summary("t", "orb", "p", "x", &summarize(&[]), None);
        let line = serde_json::to_string(&t).unwrap() + "\n";
        std::fs::write(&path, line.repeat(n)).unwrap();
        path
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
        let (lo, hi) = bootstrap_expectancy(&d, 2_000, 0.05, 1).unwrap();
        assert!((lo - 0.5).abs() < 1e-12 && (hi - 0.5).abs() < 1e-12);
    }

    #[test]
    fn a_stricter_level_widens_the_interval() {
        let d: Vec<Vec<f64>> = (0..60)
            .map(|i| vec![if i % 3 == 0 { -1.0 } else { 1.0 }])
            .collect();
        let (lo1, _) = bootstrap_expectancy(&d, resamples_for(0.05), 0.05, 3).unwrap();
        let (lo20, _) =
            bootstrap_expectancy(&d, resamples_for(0.05 / 20.0), 0.05 / 20.0, 3).unwrap();
        assert!(lo20 < lo1);
    }

    #[test]
    fn too_few_resamples_for_the_level_gives_no_interval() {
        let d = vec![vec![1.0], vec![-1.0]];
        // 2,000 draws can't support alpha = 0.05 / 1000 (each tail would hold 0.05 draws)
        assert_eq!(bootstrap_expectancy(&d, 2_000, 0.05 / 1000.0, 1), None);
        assert!(bootstrap_expectancy(&d, resamples_for(0.05 / 1000.0), 0.05 / 1000.0, 1).is_some());
        assert_eq!(resamples_for(0.05), 2_000);
        assert_eq!(resamples_for(0.0005), 80_000);
    }

    #[test]
    fn bootstrap_needs_trades_and_sane_inputs() {
        assert_eq!(bootstrap_expectancy(&[], 2_000, 0.05, 1), None);
        assert_eq!(
            bootstrap_expectancy(&[vec![], vec![]], 2_000, 0.05, 1),
            None
        );
        assert_eq!(bootstrap_expectancy(&[vec![1.0]], 0, 0.05, 1), None);
        assert_eq!(bootstrap_expectancy(&[vec![1.0]], 2_000, 1.5, 1), None);
        assert_eq!(bootstrap_expectancy(&[vec![1.0]], 2_000, 0.0, 1), None);
        assert_eq!(bootstrap_expectancy(&[vec![1.0]], 2_000, f64::NAN, 1), None);
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
        for (train, test) in walk_forward(250, 120, 21) {
            assert!(train.end == test.start && !test.is_empty());
        }
    }

    #[test]
    fn trial_log_appends_counts_and_refuses_corruption() {
        let path = log_with("append", 0);
        let mut t = Trial::from_summary(
            "now",
            "orb",
            "default",
            "2020..2024",
            &summarize(&[]),
            Some(-0.05),
        );
        assert_eq!(count_trials(&path), Ok(0));
        assert_eq!(record_trial(&path, &t), Ok(1));
        t.profit_factor = None; // no losing trades: must still round-trip
        assert_eq!(record_trial(&path, &t), Ok(2));
        t.expectancy_r = f64::NAN;
        assert!(record_trial(&path, &t).is_err());
        assert_eq!(count_trials(&path), Ok(2), "the bad trial wasn't written");
        std::fs::write(&path, "{not json}\n").unwrap();
        assert!(count_trials(&path).is_err());
    }

    #[test]
    fn an_infinite_profit_factor_is_logged_as_none() {
        let mut s = summarize(&days(&[&[100]]));
        assert!(s.profit_factor.is_infinite());
        let t = Trial::from_summary("t", "orb", "p", "x", &s, None);
        assert_eq!(t.profit_factor, None);
        s.profit_factor = 1.5;
        assert_eq!(
            Trial::from_summary("t", "orb", "p", "x", &s, None).profit_factor,
            Some(1.5)
        );
    }

    /// 6 folds of 10 days, each with a +0.25R mean (2 trades a day).
    fn good_folds() -> Vec<Vec<DayResult>> {
        (0..6).map(|_| days(&[&[150i64, -100][..]; 10])).collect()
    }

    #[test]
    fn the_gate_passes_only_when_every_check_does() {
        let base = days(&[&[-100i64, 80][..]; 60]); // mean −0.1R
        let hold = days(&[&[150i64, -100][..]; 20]); // 40 trades
        let log = log_with("gate", 1);
        let r = p1_gate(&good_folds(), &base, Some(&hold), &log).unwrap();
        assert!(r.passed(), "{r:#?}");
        assert!(
            !p1_gate(&good_folds(), &base, None, &log).unwrap().passed(),
            "needs the holdout"
        );
        let short = days(&[&[150i64, -100][..]; 5]); // 10 trades
        assert!(
            !p1_gate(&good_folds(), &base, Some(&short), &log)
                .unwrap()
                .passed(),
            "holdout too small"
        );
        let worse: Vec<Vec<DayResult>> = vec![base.clone()];
        assert!(
            !p1_gate(&worse, &good_folds()[0], Some(&hold), &log)
                .unwrap()
                .passed()
        );
        assert!(
            !p1_gate(&[], &base, Some(&hold), &log).unwrap().passed(),
            "no trades"
        );
    }

    #[test]
    fn one_lucky_window_cannot_carry_the_result() {
        // 1 great fold, 4 losing ones: pooled numbers might look fine, the fold check doesn't
        let mut folds = vec![days(&[&[400i64, 400][..]; 30])];
        folds.extend((0..4).map(|_| days(&[&[-100i64][..]; 10])));
        let r = p1_gate(
            &folds,
            &days(&[&[-100i64][..]; 60]),
            None,
            &log_with("lucky", 1),
        )
        .unwrap();
        let fold = r
            .checks
            .iter()
            .find(|c| c.name == "most walk-forward windows profitable")
            .unwrap();
        assert!(!fold.passed, "{fold:?}");
    }

    #[test]
    fn the_gate_needs_a_logged_trial_and_tightens_with_more() {
        assert!(p1_gate(&good_folds(), &[], None, &log_with("none", 0)).is_err());
        // a noisy edge: 3 wins, 2 losses of 1R, repeated
        let noisy: Vec<&[i64]> = (0..120)
            .map(|i| {
                if i % 5 < 3 {
                    &[100i64][..]
                } else {
                    &[-100i64][..]
                }
            })
            .collect();
        let folds = vec![days(&noisy)];
        let base = days(&[&[-100i64, 80][..]; 60]);
        let ci_passes = |trials| {
            p1_gate(
                &folds,
                &base,
                None,
                &log_with(&format!("n{trials}"), trials),
            )
            .unwrap()
            .checks[0]
                .passed
        };
        assert!(ci_passes(1), "one trial: the interval clears zero");
        assert!(
            !ci_passes(200),
            "after 200 trials the same evidence isn't enough"
        );
        let r = p1_gate(&folds, &base, None, &log_with("n10000", 10_000)).unwrap();
        assert!(
            r.checks[0].detail.contains("too many trials"),
            "{}",
            r.checks[0].detail
        );
    }
}
