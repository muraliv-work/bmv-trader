//! Power check for trials 11 and 12 (research plan step 2): the smallest effect each could detect,
//! before either exists. The spread comes from trial 7's months, already spent on its own run, so
//! nothing here looks at new data. Only spreads are reported, never a mean of anything new.

use crate::momentum::{self, Month};
use std::collections::BTreeMap;

/// Standard normal quantile (Acklam's rational approximation, |error| < 1.2e-9).
pub fn z(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969683028665376e1,
        2.209460984245205e2,
        -2.759285104469687e2,
        1.38357751867269e2,
        -3.066479806614716e1,
        2.506628277459239,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e1,
        1.615858368580409e2,
        -1.556989798598866e2,
        6.680131188771972e1,
        -1.328068155288572e1,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-3,
        -3.223964580411365e-1,
        -2.400758277161838,
        -2.549732539343734,
        4.374664141464968,
        2.938163982698783,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-3,
        3.224671290700398e-1,
        2.445134137142996,
        3.754408661907416,
    ];
    let tail = |q: f64| {
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    if p < 0.02425 {
        tail((-2.0 * p.ln()).sqrt())
    } else if p > 1.0 - 0.02425 {
        -tail((-2.0 * (1.0 - p).ln()).sqrt())
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    }
}

/// Sample standard deviation.
pub fn sd(x: &[f64]) -> f64 {
    let m = momentum::mean(x);
    (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len().max(2) - 1) as f64).sqrt()
}

/// Standard error of a mean over `months`, from the spread of `block`-month means in `series`
/// (blocks keep the autocorrelation a bootstrap of blocks would keep).
pub fn block_se(series: &[f64], block: usize, months: usize) -> f64 {
    let means: Vec<f64> = series.chunks_exact(block).map(momentum::mean).collect();
    sd(&means) / (months as f64 / block as f64).sqrt()
}

/// The smallest true mean the gate passes with probability `power`: the gate is a two-sided
/// interval at 1 − 0.05/`trials` whose lower bound must clear 0.
pub fn detectable(se: f64, trials: usize, power: f64) -> f64 {
    let alpha = crate::stats::FAMILY_ALPHA / trials as f64;
    (z(1.0 - alpha / 2.0) + z(power)) * se
}

/// Average ranks (ties share their mean rank), 1-based.
fn ranks(x: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..x.len()).collect();
    idx.sort_by(|&a, &b| x[a].total_cmp(&x[b]));
    let mut out = vec![0.0; x.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && x[idx[j + 1]] == x[idx[i]] {
            j += 1;
        }
        for &k in &idx[i..=j] {
            out[k] = (i + j) as f64 / 2.0 + 1.0;
        }
        i = j + 1;
    }
    out
}

/// Spearman rank correlation; None under 3 pairs or with no variation.
pub fn spearman(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.len() != b.len() || a.len() < 3 {
        return None;
    }
    let (ra, rb) = (ranks(a), ranks(b));
    let (ma, mb) = (momentum::mean(&ra), momentum::mean(&rb));
    let cov: f64 = ra.iter().zip(&rb).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = ra.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = rb.iter().map(|y| (y - mb).powi(2)).sum();
    (va > 0.0 && vb > 0.0).then(|| cov / (va * vb).sqrt())
}

/// Each research month's cross-sectional IC of the 12-1 score against the next month's return.
pub fn monthly_ic(
    by_symbol: &BTreeMap<String, Vec<Month>>,
    months: std::ops::Range<usize>,
) -> Vec<f64> {
    months
        .filter_map(|k| {
            let names = momentum::eligible(by_symbol, k);
            let score: Vec<f64> = names.iter().map(|n| n.1).collect();
            let ret: Vec<f64> = names.iter().map(|n| n.2).collect();
            spearman(&score, &ret)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_quantiles() {
        for (p, want) in [
            (0.5, 0.0),
            (0.975, 1.959964),
            (0.8, 0.841621),
            (0.99886, 3.0518),
        ] {
            assert!((z(p) - want).abs() < 1e-3, "z({p}) = {}", z(p));
        }
        assert!((z(0.025) + z(0.975)).abs() < 1e-9);
    }

    #[test]
    fn detectable_effect_scales_with_the_spread_and_the_bar() {
        // 1.96 + 0.84 standard errors at one trial and 80% power
        assert!((detectable(1.0, 1, 0.8) - 2.8016).abs() < 1e-3);
        assert!(detectable(1.0, 11, 0.8) > detectable(1.0, 1, 0.8));
        let flat = [0.01; 12];
        assert_eq!(block_se(&flat, 3, 12), 0.0);
        // block means 1, 2, 3, 4: sd 1.29, four blocks
        let s = [1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 3.0, 3.0, 3.0, 4.0, 4.0, 4.0];
        assert!((block_se(&s, 3, 12) - 1.290994 / 2.0).abs() < 1e-6);
        assert!((block_se(&s, 3, 48) - 1.290994 / 4.0).abs() < 1e-6);
    }

    #[test]
    fn spearman_uses_ranks_and_shares_ties() {
        assert_eq!(ranks(&[10.0, 30.0, 20.0, 20.0]), [1.0, 4.0, 2.5, 2.5]);
        assert!((spearman(&[1.0, 2.0, 3.0], &[10.0, 100.0, 1000.0]).unwrap() - 1.0).abs() < 1e-12);
        assert!((spearman(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]).unwrap() + 1.0).abs() < 1e-12);
        assert_eq!(spearman(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), None);
        assert_eq!(spearman(&[1.0, 2.0], &[1.0, 2.0]), None);
    }
}
