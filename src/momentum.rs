//! Trial 7: 12-1 month momentum on adjusted daily bars (DESIGN §13c). Each month, hold the 20
//! liquid stocks that rose most from 12 months to 1 month ago; compare with 20 random eligible
//! stocks and with SPY. Research only: the engine can't hold overnight yet.

use crate::scan::DailyBar;
use crate::stats::{self, GateCheck, GateReport};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The pre-registered rules (DESIGN §13c). Changing any of these is a new trial.
pub const LOOKBACK: usize = 252;
pub const SKIP: usize = 21;
pub const LIQUIDITY_SESSIONS: usize = 63;
pub const MIN_LIQUIDITY_BARS: usize = 60;
pub const MIN_DOLLAR_VOLUME: f64 = 20_000_000.0;
pub const HOLD: usize = 20;
pub const COST_PER_SIDE: f64 = 0.002;
pub const RANDOM_SEEDS: u64 = 20;
pub const BENCHMARK: &str = "SPY";
pub const MIN_POSITIVE_YEARS: f64 = 0.6;
pub const MAX_DRAWDOWN_VS_BENCHMARK: f64 = 1.5;
/// Confidence bounds resample blocks of this many consecutive months: momentum crashes cluster.
pub const BLOCK_MONTHS: usize = 3;
/// The pre-registered download (`fetch-daily --adjusted`); a run on any other range is refused.
pub const DATA_FROM: &str = "2016-01-01";
pub const DATA_TO: &str = "2026-10-30";
/// The name trial 7 is logged under; a second run is refused, so it stays one trial.
pub const TRIAL_NAME: &str = "momentum 12-1 top20";

/// Trading sessions (the benchmark's bar dates) and the first session of each month.
pub struct Calendar {
    pub sessions: Vec<String>,
    index: HashMap<String, usize>,
    /// Session indices of rebalance days. The first session loaded is never one: its month may
    /// have started before the data.
    pub rebalances: Vec<usize>,
}

impl Calendar {
    /// Errors unless the dates are YYYY-MM-DD and strictly increasing: a gap or repeat would shift
    /// every lookback.
    pub fn new(benchmark: &[DailyBar]) -> Result<Self, String> {
        let sessions: Vec<String> = benchmark.iter().map(|b| b.date.clone()).collect();
        for w in sessions.windows(2) {
            if w[0] >= w[1] {
                return Err(format!(
                    "benchmark dates out of order: {} then {}",
                    w[0], w[1]
                ));
            }
        }
        if let Some(bad) = sessions
            .iter()
            .find(|d| crate::history::canonical_date(d).is_err())
        {
            return Err(format!("benchmark date {bad:?} is not YYYY-MM-DD"));
        }
        let rebalances = (1..sessions.len())
            .filter(|&i| sessions[i][..7] != sessions[i - 1][..7])
            .collect();
        let index = sessions
            .iter()
            .enumerate()
            .map(|(i, d)| (d.clone(), i))
            .collect();
        Ok(Calendar {
            sessions,
            index,
            rebalances,
        })
    }

    /// A symbol's usable bars by session index; sessions it didn't trade (or bars off the
    /// calendar) are None.
    pub(crate) fn aligned<'a>(&self, bars: &'a [DailyBar]) -> Vec<Option<&'a DailyBar>> {
        let mut on = vec![None; self.sessions.len()];
        for b in bars.iter().filter(|b| usable(b)) {
            if let Some(&i) = self.index.get(&b.date) {
                on[i] = Some(b);
            }
        }
        on
    }

    pub fn date(&self, month: usize) -> &str {
        &self.sessions[self.rebalances[month]]
    }

    /// The date a month's positions are sold: the next rebalance day.
    pub fn exit_date(&self, month: usize) -> Option<&str> {
        let i = *self.rebalances.get(month + 1)?;
        Some(&self.sessions[i])
    }
}

/// One symbol in one month: its score if it was eligible on the signal day, and its open-to-open
/// return if it traded on the rebalance day.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Month {
    pub score: Option<f64>,
    pub ret: Option<f64>,
}

/// A bar with a price that can't be traded or divided by is treated as missing.
fn usable(b: &DailyBar) -> bool {
    b.open.is_finite() && b.open > 0.0 && b.close.is_finite() && b.close > 0.0
}

/// A symbol's score and return for every month of the calendar.
pub fn months(bars: &[DailyBar], cal: &Calendar) -> Vec<Month> {
    let on = cal.aligned(bars);
    (0..cal.rebalances.len())
        .map(|k| {
            let r = cal.rebalances[k];
            Month {
                score: score(&on, r - 1),
                ret: cal
                    .rebalances
                    .get(k + 1)
                    .and_then(|&next| held_return(&on, r, next)),
            }
        })
        .collect()
}

/// close(s−21) / close(s−252) − 1, if the symbol is eligible on signal session `s`.
fn score(on: &[Option<&DailyBar>], s: usize) -> Option<f64> {
    if s < LOOKBACK || s + 1 < LIQUIDITY_SESSIONS {
        return None;
    }
    on[s]?;
    let (recent, past) = (on[s - SKIP]?, on[s - LOOKBACK]?);
    liquid(on, s).then(|| recent.close / past.close - 1.0)
}

/// At least $20M average dollar volume over the 63 sessions ending at `s`, with bars on at
/// least 60 of them (shared by trials 7 and 8).
pub(crate) fn liquid(on: &[Option<&DailyBar>], s: usize) -> bool {
    if s + 1 < LIQUIDITY_SESSIONS {
        return false;
    }
    let window: Vec<&DailyBar> = on[s + 1 - LIQUIDITY_SESSIONS..=s]
        .iter()
        .flatten()
        .copied()
        .collect();
    let dollars = window
        .iter()
        .map(|b| b.close * b.volume as f64)
        .sum::<f64>();
    window.len() >= MIN_LIQUIDITY_BARS && dollars / window.len() as f64 >= MIN_DOLLAR_VOLUME
}

/// Bought at the open of session `entry`, sold at the open of `exit`; if it has no bar there
/// (delisted), sold at its last close before.
fn held_return(on: &[Option<&DailyBar>], entry: usize, exit: usize) -> Option<f64> {
    let buy = on[entry]?.open;
    let sell = match on[exit] {
        Some(b) => b.open,
        None => on[entry..exit].iter().rev().flatten().next()?.close,
    };
    Some(sell / buy - 1.0)
}

/// Eligible names in month `k`: (symbol, score, return), in symbol order.
fn eligible(by_symbol: &BTreeMap<String, Vec<Month>>, k: usize) -> Vec<(&str, f64, f64)> {
    by_symbol
        .iter()
        .filter_map(|(s, m)| match m.get(k)? {
            Month {
                score: Some(sc),
                ret: Some(r),
            } => Some((s.as_str(), *sc, *r)),
            _ => None,
        })
        .collect()
}

/// The top `HOLD` by score, ties broken by symbol.
pub fn top(names: &[(&str, f64, f64)]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..names.len()).collect();
    idx.sort_by(|&a, &b| {
        names[b]
            .1
            .total_cmp(&names[a].1)
            .then_with(|| names[a].0.cmp(names[b].0))
    });
    idx.truncate(HOLD);
    idx
}

/// `HOLD` names drawn without replacement (xorshift, so a seed always gives the same draws).
fn random(names: &[(&str, f64, f64)], rng: &mut u64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..names.len()).collect();
    let n = HOLD.min(idx.len());
    for i in 0..n {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        let j = i + (*rng % (idx.len() - i) as u64) as usize;
        idx.swap(i, j);
    }
    idx.truncate(n);
    idx
}

/// Picks indices from a month's eligible (symbol, score, return) list.
pub type Chooser<'a> = dyn FnMut(&[(&str, f64, f64)]) -> Vec<usize> + 'a;

/// Net return of each month in `months`, holding what `choose` picks at equal weight. Costs are
/// charged on the weight of names entering and leaving; a month with nothing eligible is cash.
pub fn simulate(
    by_symbol: &BTreeMap<String, Vec<Month>>,
    months: std::ops::Range<usize>,
    choose: &mut Chooser<'_>,
) -> Vec<f64> {
    let mut held: BTreeSet<String> = BTreeSet::new();
    months
        .map(|k| {
            let names = eligible(by_symbol, k);
            let picks = choose(&names);
            let now: BTreeSet<String> = picks.iter().map(|&i| names[i].0.to_string()).collect();
            let gross = if picks.is_empty() {
                0.0
            } else {
                picks.iter().map(|&i| names[i].2).sum::<f64>() / picks.len() as f64
            };
            let side = |moved: usize, of: usize| {
                if of == 0 {
                    0.0
                } else {
                    moved as f64 / of as f64
                }
            };
            let cost = COST_PER_SIDE
                * (side(now.difference(&held).count(), now.len())
                    + side(held.difference(&now).count(), held.len()));
            held = now;
            gross - cost
        })
        .collect()
}

/// The research result (DESIGN §13c): momentum, the random baseline and the benchmark, month by
/// month over the same research months.
#[derive(Clone, Debug, PartialEq)]
pub struct Study {
    pub dates: Vec<String>,
    pub strategy: Vec<f64>,
    pub baseline: Vec<f64>,
    pub benchmark: Vec<f64>,
    /// Months with fewer than `HOLD` eligible names, held at equal weight over what there was.
    pub thin_months: usize,
    /// How many months momentum held each name: shows whether a few names drive the result.
    pub held: BTreeMap<String, usize>,
}

/// Runs the trial over research months only: from the first month with `HOLD` eligible names to
/// the last month sold before `holdout_from`. Holdout months are never returned.
pub fn study(
    by_symbol: &BTreeMap<String, Vec<Month>>,
    cal: &Calendar,
    holdout_from: &str,
) -> Result<Study, String> {
    let n = cal.rebalances.len();
    let first = (0..n)
        .find(|&k| eligible(by_symbol, k).len() >= HOLD)
        .ok_or("no month has 20 eligible names: is the data downloaded?")?;
    let end = (first..n)
        .take_while(|&k| cal.exit_date(k).is_some_and(|d| d < holdout_from))
        .last()
        .map(|k| k + 1)
        .ok_or("no research month ends before the holdout")?;
    let spy = by_symbol.get(BENCHMARK).ok_or("no benchmark bars (SPY)")?;
    let benchmark = (first..end)
        .map(|k| {
            spy[k]
                .ret
                .ok_or_else(|| format!("SPY has no return for {}", cal.date(k)))
        })
        .collect::<Result<Vec<f64>, String>>()?;
    let (mut thin_months, mut held) = (0, BTreeMap::new());
    let strategy = simulate(by_symbol, first..end, &mut |c| {
        let t = top(c);
        thin_months += usize::from(c.len() < HOLD);
        for &i in &t {
            *held.entry(c[i].0.to_string()).or_default() += 1;
        }
        t
    });
    let mut baseline = vec![0.0; end - first];
    for seed in 1..=RANDOM_SEEDS {
        let mut rng = splitmix(seed);
        for (b, r) in baseline
            .iter_mut()
            .zip(simulate(by_symbol, first..end, &mut |c| {
                random(c, &mut rng)
            }))
        {
            *b += r / RANDOM_SEEDS as f64;
        }
    }
    Ok(Study {
        dates: (first..end).map(|k| cal.date(k).to_string()).collect(),
        strategy,
        baseline,
        benchmark,
        thin_months,
        held,
    })
}

/// Spreads small seeds over all 64 bits, so seeds 1..20 don't start out correlated.
fn splitmix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)).max(1)
}

pub fn mean(r: &[f64]) -> f64 {
    r.iter().sum::<f64>() / r.len().max(1) as f64
}

/// Compound annual growth rate of monthly returns.
pub fn cagr(r: &[f64]) -> f64 {
    let growth: f64 = r.iter().map(|x| 1.0 + x).product();
    growth.powf(12.0 / r.len().max(1) as f64) - 1.0
}

/// Annualised Sharpe ratio of monthly returns (no risk-free rate); 0 with no variation.
pub fn sharpe(r: &[f64]) -> f64 {
    if r.len() < 2 {
        return 0.0;
    }
    let m = mean(r);
    let var = r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (r.len() - 1) as f64;
    if var > 0.0 {
        m / var.sqrt() * 12f64.sqrt()
    } else {
        0.0
    }
}

/// Largest peak-to-trough fall of the compounded equity, as a fraction.
pub fn max_drawdown(r: &[f64]) -> f64 {
    let (mut equity, mut peak, mut worst) = (1.0f64, 1.0f64, 0.0f64);
    for x in r {
        equity *= 1.0 + x;
        peak = peak.max(equity);
        worst = worst.max(1.0 - equity / peak);
    }
    worst
}

/// Sum of winning months over the sum of losing months; None with no losing month.
pub fn profit_factor(r: &[f64]) -> Option<f64> {
    let win: f64 = r.iter().filter(|x| **x > 0.0).sum();
    let loss: f64 = -r.iter().filter(|x| **x < 0.0).sum::<f64>();
    (loss > 0.0).then(|| win / loss)
}

/// Confidence bounds of the mean monthly value, resampling blocks of `BLOCK_MONTHS` months.
fn ci(r: &[f64], iters: usize, alpha: f64) -> Option<(f64, f64)> {
    let blocks: Vec<Vec<f64>> = r.chunks(BLOCK_MONTHS).map(<[f64]>::to_vec).collect();
    stats::bootstrap_expectancy(&blocks, iters, alpha, 42)
}

impl Study {
    pub fn excess(&self) -> Vec<f64> {
        self.strategy
            .iter()
            .zip(&self.baseline)
            .map(|(s, b)| s - b)
            .collect()
    }

    /// Lower bound of the mean monthly return, at the level `trials` sets.
    pub fn ci_low(&self, trials: usize) -> Option<f64> {
        let alpha = stats::FAMILY_ALPHA / trials.max(1) as f64;
        ci(&self.strategy, stats::resamples_for(alpha), alpha).map(|(lo, _)| lo)
    }

    /// The trial 7 gate (DESIGN §13c); `trials` includes this one.
    pub fn gate(&self, trials: usize) -> GateReport {
        let alpha = stats::FAMILY_ALPHA / trials.max(1) as f64;
        let iters = stats::resamples_for(alpha);
        let level = format!("{:.2}% two-sided, {trials} trials", 100.0 * (1.0 - alpha));
        let too_many = iters > stats::MAX_RESAMPLES;
        let bound = |r: &[f64], name: &'static str| {
            let c = if too_many { None } else { ci(r, iters, alpha) };
            GateCheck {
                name,
                passed: c.is_some_and(|(lo, _)| lo > 0.0),
                detail: match c {
                    Some((lo, hi)) => format!(
                        "{:+.2}% .. {:+.2}% a month at {level}",
                        lo * 100.0,
                        hi * 100.0
                    ),
                    None => format!("no interval ({level})"),
                },
            }
        };
        let excess = self.excess();
        // each year's compounded growth, momentum vs random; partial first and last years count
        let mut years: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
        for ((d, s), b) in self.dates.iter().zip(&self.strategy).zip(&self.baseline) {
            let y = years.entry(&d[..4]).or_insert((1.0, 1.0));
            y.0 *= 1.0 + s;
            y.1 *= 1.0 + b;
        }
        let good_years = years.values().filter(|(s, b)| s > b).count();
        let (dd, spy_dd) = (max_drawdown(&self.strategy), max_drawdown(&self.benchmark));
        let pct = |x: f64| format!("{:+.2}%", x * 100.0);
        GateReport {
            checks: vec![
                bound(&self.strategy, "lower bound of mean monthly return > 0"),
                bound(&excess, "lower bound of mean excess over random > 0"),
                GateCheck {
                    name: "CAGR above SPY",
                    passed: cagr(&self.strategy) > cagr(&self.benchmark),
                    detail: format!(
                        "{} vs {}",
                        pct(cagr(&self.strategy)),
                        pct(cagr(&self.benchmark))
                    ),
                },
                GateCheck {
                    name: "Sharpe above SPY",
                    passed: sharpe(&self.strategy) > sharpe(&self.benchmark),
                    detail: format!(
                        "{:.2} vs {:.2}",
                        sharpe(&self.strategy),
                        sharpe(&self.benchmark)
                    ),
                },
                GateCheck {
                    name: "at least 60% of years beat random",
                    passed: !years.is_empty()
                        && good_years as f64 >= MIN_POSITIVE_YEARS * years.len() as f64,
                    detail: format!("{good_years}/{}", years.len()),
                },
                GateCheck {
                    name: "max drawdown at most 1.5x SPY's",
                    passed: dd <= MAX_DRAWDOWN_VS_BENCHMARK * spy_dd,
                    detail: format!("{:.1}% vs SPY {:.1}%", dd * 100.0, spy_dd * 100.0),
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(date: &str, open: f64, close: f64, volume: u64) -> DailyBar {
        DailyBar {
            date: date.into(),
            open,
            high: open.max(close),
            low: open.min(close),
            close,
            volume,
        }
    }

    /// `n` weekday-ish sessions, 20 per month, named YYYY-MM-DD.
    fn dates(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                format!(
                    "{:04}-{:02}-{:02}",
                    2016 + i / 240,
                    i % 240 / 20 + 1,
                    i % 20 + 1
                )
            })
            .collect()
    }

    fn flat(ds: &[String], price: f64, volume: u64) -> Vec<DailyBar> {
        ds.iter().map(|d| bar(d, price, price, volume)).collect()
    }

    #[test]
    fn calendar_rebalances_on_each_new_month_but_not_the_first_day() {
        let cal = Calendar::new(&flat(&dates(60), 1.0, 1)).unwrap();
        assert_eq!(cal.rebalances, vec![20, 40]);
        assert_eq!(cal.date(0), "2016-02-01");
        assert_eq!(cal.exit_date(0), Some("2016-03-01"));
        assert_eq!(cal.exit_date(1), None);
    }

    #[test]
    fn score_is_12_minus_1_month_return_and_needs_liquidity_and_history() {
        let ds = dates(300);
        let cal = Calendar::new(&flat(&ds, 1.0, 1)).unwrap();
        // the rebalance at session 260 (month 13), signal day 259
        let k = cal.rebalances.iter().position(|&r| r == 260).unwrap();
        let mut bars = flat(&ds, 10.0, 3_000_000); // $30M a day
        bars[259 - LOOKBACK].close = 5.0;
        bars[259 - SKIP].close = 15.0;
        let m = months(&bars, &cal);
        assert_eq!(m[k].score, Some(2.0), "15 / 5 - 1");
        let early = cal.rebalances.iter().position(|&r| r == 240).unwrap();
        assert_eq!(
            m[early].score, None,
            "signal day 239 has no 252 sessions behind it"
        );

        let thin = flat(&ds, 10.0, 1_000_000); // $10M a day
        assert_eq!(months(&thin, &cal)[k].score, None);

        let mut gappy = bars.clone();
        gappy.remove(259 - SKIP);
        assert_eq!(
            months(&gappy, &cal)[k].score,
            None,
            "no bar 21 sessions back"
        );

        let mut sparse = bars.clone();
        for i in [250, 251, 252, 253] {
            sparse[i].volume = 0;
            sparse[i].close = 0.0; // unusable, so missing: 59 of 63 bars
        }
        assert_eq!(months(&sparse, &cal)[k].score, None);
    }

    #[test]
    fn return_is_open_to_open_or_last_close_when_delisted() {
        let ds = dates(60);
        let cal = Calendar::new(&flat(&ds, 1.0, 1)).unwrap();
        let mut bars = flat(&ds, 10.0, 1);
        bars[20].open = 8.0;
        bars[40].open = 12.0;
        let m = months(&bars, &cal);
        assert!((m[0].ret.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(m[1].ret, None, "the last month has no exit yet");

        let mut gone = bars[..30].to_vec();
        gone[29].close = 4.0;
        assert!((months(&gone, &cal)[0].ret.unwrap() - (4.0 / 8.0 - 1.0)).abs() < 1e-12);

        let late = bars[21..].to_vec();
        assert_eq!(
            months(&late, &cal)[0].ret,
            None,
            "no bar on the rebalance day"
        );
    }

    #[test]
    fn top_takes_best_scores_and_breaks_ties_by_symbol() {
        let syms: Vec<String> = (0..25).map(|i| format!("S{i:02}")).collect();
        let mut names: Vec<(&str, f64, f64)> =
            syms.iter().map(|s| (s.as_str(), 0.0, 0.0)).collect();
        names[24].1 = 1.0;
        let t = top(&names);
        assert_eq!(t.len(), HOLD);
        assert_eq!(t[0], 24);
        assert_eq!(t[1], 0, "ties in symbol order");
        assert!(!t.contains(&23));
    }

    fn universe(rets: &[(&str, f64, f64)], months: usize) -> BTreeMap<String, Vec<Month>> {
        rets.iter()
            .map(|(s, sc, r)| {
                (
                    s.to_string(),
                    vec![
                        Month {
                            score: Some(*sc),
                            ret: Some(*r),
                        };
                        months
                    ],
                )
            })
            .collect()
    }

    #[test]
    fn simulate_charges_costs_only_on_names_that_change() {
        let syms: Vec<String> = (0..20).map(|i| format!("S{i:02}")).collect();
        let rows: Vec<(&str, f64, f64)> = syms.iter().map(|s| (s.as_str(), 1.0, 0.01)).collect();
        let u = universe(&rows, 3);
        let r = simulate(&u, 0..3, &mut |c| top(c));
        assert!((r[0] - (0.01 - COST_PER_SIDE)).abs() < 1e-12, "buying in");
        assert!((r[1] - 0.01).abs() < 1e-12, "same names, no cost");
        let mut swapped = u.clone();
        swapped.get_mut("S00").unwrap()[2].score = None;
        swapped.insert(
            "Z".into(),
            vec![
                Month {
                    score: Some(2.0),
                    ret: Some(0.01)
                };
                3
            ],
        );
        let r = simulate(&swapped, 0..3, &mut |c| top(c));
        let expect = 0.01 - COST_PER_SIDE * (1.0 / 20.0 + 1.0 / 20.0);
        assert!((r[2] - expect).abs() < 1e-12, "one in, one out");
        let empty = simulate(&BTreeMap::new(), 0..1, &mut |c| top(c));
        assert_eq!(empty, vec![0.0], "nothing eligible is cash");
    }

    #[test]
    fn random_draws_distinct_names_and_repeats_for_a_seed() {
        let syms: Vec<String> = (0..50).map(|i| format!("S{i:02}")).collect();
        let names: Vec<(&str, f64, f64)> = syms.iter().map(|s| (s.as_str(), 0.0, 0.0)).collect();
        let (mut a, mut b) = (7u64, 7u64);
        let x = random(&names, &mut a);
        assert_eq!(x, random(&names, &mut b));
        assert_eq!(x.len(), HOLD);
        assert_eq!(x.iter().collect::<BTreeSet<_>>().len(), HOLD);
        assert_eq!(random(&names[..5], &mut a).len(), 5);
    }

    #[test]
    fn metrics() {
        assert!((cagr(&[0.01; 12]) - (1.01f64.powi(12) - 1.0)).abs() < 1e-12);
        assert!((max_drawdown(&[0.1, -0.5, 0.2]) - 0.5).abs() < 1e-12);
        assert_eq!(max_drawdown(&[0.1, 0.1]), 0.0);
        assert_eq!(profit_factor(&[0.1, 0.1]), None);
        assert!((profit_factor(&[0.3, -0.1]).unwrap() - 3.0).abs() < 1e-12);
        assert_eq!(sharpe(&[0.01, 0.01]), 0.0);
        assert!(sharpe(&[0.02, 0.0, 0.02, 0.0]) > 0.0);
    }

    #[test]
    fn study_stops_before_the_holdout_and_gate_reads_it() {
        let ds = dates(400);
        let cal = Calendar::new(&flat(&ds, 1.0, 1)).unwrap();
        let n = cal.rebalances.len();
        let mut u = BTreeMap::new();
        for i in 0..25 {
            let m: Vec<Month> = (0..n)
                .map(|k| Month {
                    score: (k >= 2).then_some(i as f64),
                    ret: Some(if i >= 5 { 0.03 } else { -0.01 }),
                })
                .collect();
            u.insert(format!("S{i:02}"), m);
        }
        u.insert(
            BENCHMARK.into(),
            vec![
                Month {
                    score: None,
                    ret: Some(0.005)
                };
                n
            ],
        );
        let holdout = cal.date(15).to_string();
        let s = study(&u, &cal, &holdout).unwrap();
        assert_eq!(
            s.dates.first().unwrap(),
            cal.date(2),
            "first month with 20 eligible"
        );
        assert_eq!(
            s.dates.last().unwrap(),
            cal.date(13),
            "sold by the holdout start"
        );
        assert!(s.dates.iter().all(|d| d.as_str() < holdout.as_str()));
        assert!(s.strategy[1] > s.baseline[1], "top 20 are the 3% names");
        let g = s.gate(7);
        assert!(
            g.checks
                .iter()
                .find(|c| c.name.starts_with("CAGR"))
                .unwrap()
                .passed
        );
        assert!(study(&BTreeMap::new(), &cal, &holdout).is_err());
    }

    #[test]
    fn calendar_refuses_out_of_order_or_malformed_dates() {
        let mut swapped = flat(&dates(5), 1.0, 1);
        swapped.swap(1, 2);
        assert!(Calendar::new(&swapped).is_err());
        let mut repeated = flat(&dates(5), 1.0, 1);
        repeated[4].date = repeated[3].date.clone();
        assert!(Calendar::new(&repeated).is_err());
        let mut bad = flat(&dates(3), 1.0, 1);
        bad[2].date = "2016-01-99".into();
        assert!(Calendar::new(&bad).is_err());
    }

    #[test]
    fn gate_fails_when_momentum_is_no_better_than_random() {
        let months = 36;
        let strategy: Vec<f64> = (0..months)
            .map(|i| if i % 2 == 0 { 0.03 } else { -0.01 })
            .collect();
        let s = Study {
            dates: (0..months)
                .map(|i| format!("{}-{:02}-01", 2017 + i / 12, i % 12 + 1))
                .collect(),
            baseline: strategy.clone(),
            strategy,
            benchmark: vec![0.0; months],
            thin_months: 0,
            held: BTreeMap::new(),
        };
        let g = s.gate(7);
        let check = |name: &str| {
            g.checks
                .iter()
                .find(|c| c.name.starts_with(name))
                .unwrap()
                .passed
        };
        assert!(check("lower bound of mean monthly return"), "{g:?}");
        assert!(!check("lower bound of mean excess"), "no edge over random");
        assert!(!check("at least 60% of years"), "a tie is not a win");
        assert!(
            !check("max drawdown"),
            "SPY never fell, so any drawdown is too big"
        );
        assert!(!g.passed());
    }

    #[test]
    fn splitmix_spreads_small_seeds() {
        let a: BTreeSet<u64> = (1..=20).map(splitmix).collect();
        assert_eq!(a.len(), 20);
        assert!(a.iter().all(|x| *x > u64::from(u32::MAX)));
    }
}
