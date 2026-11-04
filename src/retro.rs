//! Trial 12 (DESIGN §7i): trial 10's AI veto, replayed with a local model on the untouched year.
//! This file holds the sample rule and the power check; nothing here reads the untouched year
//! unless its caller passes it in.

use crate::lab::Rng;
use crate::power;
use crate::swing::Cand;

pub const TRIAL_NAME: &str = "ai-veto retro local";
pub const TRIAL_NUMBER: usize = 12;
pub const FROM: &str = "2025-10-02";
/// The power gate (DESIGN §7i): build only if the 80%-power MDE of D is at most this, in R.
pub const MAX_MDE_R: f64 = 0.20;

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
