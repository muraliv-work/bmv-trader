//! The stress battery (build step 3, DESIGN §4c): the tests a strategy must survive before the
//! desk lets an agent promote it. Pure numbers over a trial's reproduced results; the trial runs
//! themselves live in `main.rs`. The pass bars were set by the owner before anything ran
//! (2026-11-04) and are constants here so a run can't move them.

use crate::autopsy::Row;
use crate::lab::Rng;
use serde::Serialize;

/// Monte Carlo: the 5th percentile of the resampled mean must be above this.
pub const MC_P5_MIN: f64 = 0.0;
/// Still positive at this multiple of the trial's costs.
pub const COST_SHOCK: f64 = 2.0;
/// Share of ±20% parameter neighbours whose expectancy keeps the trial's sign.
pub const NEIGHBOUR_SHARE: f64 = 0.75;
/// Share of calendar years with a positive mean.
pub const YEAR_SHARE: f64 = 2.0 / 3.0;
/// Deflated Sharpe ratio: the probability the Sharpe beats the best of N luck-only trials.
pub const DSR_MIN: f64 = 0.95;
pub const MC_DRAWS: usize = 10_000;
pub const MC_SEED: u64 = 20261104;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MonteCarlo {
    /// 5th percentile of the resampled mean.
    pub p5_mean: f64,
    /// Share of resamples with a mean below zero.
    pub p_negative: f64,
    /// 95th percentile of the worst peak-to-trough fall of the cumulative sum.
    pub p95_drawdown: f64,
}

fn mean(x: &[f64]) -> f64 {
    if x.is_empty() {
        0.0
    } else {
        x.iter().sum::<f64>() / x.len() as f64
    }
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

/// Resamples the results with replacement, `draws` times, seeded so every run gives the same answer.
pub fn monte_carlo(x: &[f64], draws: usize, seed: u64) -> Option<MonteCarlo> {
    if x.is_empty() || draws == 0 {
        return None;
    }
    let mut rng = Rng::new(seed);
    let (mut means, mut falls) = (Vec::with_capacity(draws), Vec::with_capacity(draws));
    for _ in 0..draws {
        let (mut sum, mut peak, mut worst) = (0.0f64, 0.0f64, 0.0f64);
        for _ in 0..x.len() {
            sum += x[rng.below(x.len())];
            peak = peak.max(sum);
            worst = worst.max(peak - sum);
        }
        means.push(sum / x.len() as f64);
        falls.push(worst);
    }
    means.sort_by(f64::total_cmp);
    falls.sort_by(f64::total_cmp);
    Some(MonteCarlo {
        p5_mean: quantile(&means, 0.05),
        p_negative: means.iter().filter(|m| **m < 0.0).count() as f64 / draws as f64,
        p95_drawdown: quantile(&falls, 0.95),
    })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Bucket {
    pub name: String,
    pub n: usize,
    pub mean: f64,
}

/// Mean result by calendar year of the entry date.
pub fn by_year(rows: &[Row]) -> Vec<Bucket> {
    let mut years: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
    for r in rows {
        years
            .entry(r.date.get(..4).unwrap_or("?"))
            .or_default()
            .push(r.net);
    }
    years
        .into_iter()
        .map(|(y, v)| Bucket {
            name: y.into(),
            n: v.len(),
            mean: mean(&v),
        })
        .collect()
}

/// Mean result with SPY above and below its 200-day average on the signal day.
pub fn by_regime(rows: &[Row]) -> Vec<Bucket> {
    [(true, "SPY above 200-day"), (false, "SPY below 200-day")]
        .into_iter()
        .map(|(up, name)| {
            let v: Vec<f64> = rows
                .iter()
                .filter(|r| r.up_market == Some(up))
                .map(|r| r.net)
                .collect();
            Bucket {
                name: name.into(),
                n: v.len(),
                mean: mean(&v),
            }
        })
        .collect()
}

/// Per-observation Sharpe ratio, skewness and (non-excess) kurtosis.
pub fn moments(x: &[f64]) -> Option<(f64, f64, f64)> {
    if x.len() < 3 {
        return None;
    }
    let m = mean(x);
    let var = x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / x.len() as f64;
    let sd = var.sqrt();
    if sd == 0.0 {
        return None;
    }
    let skew = x.iter().map(|v| ((v - m) / sd).powi(3)).sum::<f64>() / x.len() as f64;
    let kurt = x.iter().map(|v| ((v - m) / sd).powi(4)).sum::<f64>() / x.len() as f64;
    Some((m / sd, skew, kurt))
}

/// The standard normal CDF (Abramowitz and Stegun 7.1.26; error below 1.5e-7).
pub fn norm_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs() / std::f64::consts::SQRT_2);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erfc = poly * (-(x * x) / 2.0).exp();
    if x >= 0.0 {
        1.0 - erfc / 2.0
    } else {
        erfc / 2.0
    }
}

/// The deflated Sharpe ratio (Bailey and López de Prado, 2014): the probability that a Sharpe of
/// `sr` over `t` observations, with this skew and kurtosis, beats the best Sharpe expected from
/// `trials` tries of pure luck whose Sharpes vary with variance `var_sr`.
pub fn deflated_sharpe(sr: f64, t: usize, skew: f64, kurt: f64, trials: usize, var_sr: f64) -> f64 {
    const EULER: f64 = std::f64::consts::EULER_GAMMA;
    let n = trials.max(2) as f64;
    let best_of_luck = var_sr.max(0.0).sqrt()
        * ((1.0 - EULER) * crate::power::z(1.0 - 1.0 / n)
            + EULER * crate::power::z(1.0 - 1.0 / (n * std::f64::consts::E)));
    let spread = (1.0 - skew * sr + (kurt - 1.0) / 4.0 * sr * sr).max(1e-12);
    norm_cdf((sr - best_of_luck) * ((t.max(2) - 1) as f64).sqrt() / spread.sqrt())
}

/// One test's outcome: passed, failed, or not run (which never counts as a pass).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub test: &'static str,
    pub passed: Option<bool>,
    pub detail: String,
}

/// The verdict: passes only if every test ran and passed.
pub fn verdict(checks: &[Check]) -> bool {
    !checks.is_empty() && checks.iter().all(|c| c.passed == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(date: &str, net: f64, up: bool) -> Row {
        Row {
            date: date.into(),
            net,
            exit: "target",
            hold: None,
            up_market: Some(up),
        }
    }

    #[test]
    fn monte_carlo_is_seeded_and_sees_losers() {
        let win: Vec<f64> = (0..200)
            .map(|i| if i % 4 == 0 { -1.0 } else { 1.0 })
            .collect();
        let a = monte_carlo(&win, 2000, 7).unwrap();
        assert_eq!(
            a,
            monte_carlo(&win, 2000, 7).unwrap(),
            "same seed, same answer"
        );
        assert!(a.p5_mean > 0.3 && a.p_negative == 0.0);
        let lose: Vec<f64> = win.iter().map(|v| -v).collect();
        let b = monte_carlo(&lose, 2000, 7).unwrap();
        assert!(b.p5_mean < 0.0 && b.p_negative == 1.0 && b.p95_drawdown > 50.0);
        assert!(monte_carlo(&[], 10, 1).is_none());
    }

    #[test]
    fn splits_by_year_and_regime() {
        let rows = [
            row("2020-01-02", 1.0, true),
            row("2020-06-01", -3.0, false),
            row("2021-03-01", 2.0, true),
        ];
        let y = by_year(&rows);
        assert_eq!((y[0].name.as_str(), y[0].n, y[0].mean), ("2020", 2, -1.0));
        assert_eq!((y[1].name.as_str(), y[1].mean), ("2021", 2.0));
        let r = by_regime(&rows);
        assert_eq!((r[0].n, r[0].mean, r[1].mean), (2, 1.5, -3.0));
    }

    #[test]
    fn deflated_sharpe_punishes_many_tries() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-4);
        assert!((norm_cdf(-1.0) - 0.158_655).abs() < 1e-5);
        // a decent Sharpe over many observations, one try: near certain
        let one = deflated_sharpe(0.2, 1000, 0.0, 3.0, 2, 0.0);
        assert!(one > 0.99, "{one}");
        // the same Sharpe after a thousand tries whose Sharpes vary widely: luck explains it
        let many = deflated_sharpe(0.2, 1000, 0.0, 3.0, 1000, 0.01);
        assert!(many < 0.5, "{many}");
        assert!(deflated_sharpe(-0.1, 1000, 0.0, 3.0, 12, 0.001) < 0.01);
        let (sr, skew, kurt) = moments(&[1.0, -1.0, 1.0, -1.0, 2.0]).unwrap();
        assert!(sr > 0.0 && skew.is_finite() && kurt > 1.0);
        assert!(moments(&[1.0, 1.0, 1.0]).is_none());
    }

    #[test]
    fn a_test_not_run_is_not_a_pass() {
        let c = |p| Check {
            test: "t",
            passed: p,
            detail: String::new(),
        };
        assert!(verdict(&[c(Some(true)), c(Some(true))]));
        assert!(!verdict(&[c(Some(true)), c(None)]));
        assert!(!verdict(&[c(Some(false))]));
        assert!(!verdict(&[]));
    }
}
