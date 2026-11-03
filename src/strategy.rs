//! Strategies are pure functions: today's bars so far → maybe a Candidate. No clock, no I/O, so the
//! backtest and the live engine get identical answers from identical bars (DESIGN §4, §7).
//! Sizing is not done here; `risk.rs` sizes from entry/stop and the limits.

use crate::money::{Price, Usd, risk_per_share};

/// One 1-minute bar. `ts` is the bar's **start** (Unix seconds, UTC), as Alpaca stamps it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
}

impl Bar {
    /// Finite, positive prices with low ≤ open, close ≤ high. Float `max`/`min` silently skip NaN,
    /// so anything else must be refused before it reaches the indicators.
    pub fn is_sane(&self) -> bool {
        let prices = [self.open, self.high, self.low, self.close];
        prices.iter().all(|x| x.is_finite() && *x > 0.0)
            && self.low <= self.open.min(self.close)
            && self.open.max(self.close) <= self.high
    }
}

/// A long setup. Only constructible with stop < entry < target and an uppercase symbol
/// (so "spy" can't dodge the per-symbol and cluster limits).
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    symbol: String,
    entry: Price,
    stop: Price,
    target: Price,
    reason: String,
}

impl Candidate {
    pub fn long(
        symbol: &str,
        entry: Price,
        stop: Price,
        target: Price,
        reason: String,
    ) -> Result<Self, String> {
        let valid_symbol = (1..=10).contains(&symbol.len())
            && symbol
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.');
        if !valid_symbol {
            return Err(format!("invalid symbol {symbol:?}"));
        }
        if !(stop < entry && entry < target) {
            return Err(format!(
                "{symbol}: need stop < entry < target, got {stop} / {entry} / {target}"
            ));
        }
        Ok(Self {
            symbol: symbol.to_ascii_uppercase(),
            entry,
            stop,
            target,
            reason,
        })
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }
    pub fn entry(&self) -> Price {
        self.entry
    }
    pub fn stop(&self) -> Price {
        self.stop
    }
    pub fn target(&self) -> Price {
        self.target
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }
    /// R per share (always > 0 by construction).
    pub fn r(&self) -> Usd {
        self.entry - self.stop
    }
}

/// Opening-range-breakout settings, so research can try variants without code changes.
#[derive(Clone, Debug, PartialEq)]
pub struct OrbParams {
    pub or_minutes: i64,
    /// IEX skips quiet minutes; a range built from a few bars is meaningless.
    pub min_or_bars: usize,
    pub atr_len: usize,
    pub atr_mult: f64,
    pub target_r: f64,
    /// Minimum R as a fraction of entry (0.0015 = 0.15%).
    pub min_r_frac: f64,
    pub min_r_cents: i64,
}

impl Default for OrbParams {
    fn default() -> Self {
        Self {
            or_minutes: 15,
            min_or_bars: 10,
            atr_len: 14,
            atr_mult: 1.5,
            target_r: 2.0,
            min_r_frac: 0.0015,
            min_r_cents: 5,
        }
    }
}

impl OrbParams {
    /// Call when loading a research variant, so a typo is an error rather than a silent zero trades.
    pub fn validate(&self) -> Result<(), String> {
        let ok = (1..=120).contains(&self.or_minutes)
            && (1..=self.or_minutes as usize).contains(&self.min_or_bars)
            && self.atr_len >= 1
            && self.atr_mult.is_finite()
            && self.atr_mult > 0.0
            && self.target_r.is_finite()
            && self.target_r >= 1.0
            && self.min_r_frac.is_finite()
            && self.min_r_frac >= 0.0
            && self.min_r_cents >= 0;
        if ok {
            Ok(())
        } else {
            Err(format!("invalid ORB parameters: {self:?}"))
        }
    }
}

/// ORB long (DESIGN §7), evaluated on the **last** bar of `bars` — today's regular-hours bars so far,
/// oldest first. Fires only on the first bar after the opening range that closes above its high
/// (if that bar fails the volume filter, the day is done), with volume above the average of the
/// earlier bars today. Bad bars, out-of-order bars, or bad params mean no candidate.
pub fn orb_long(symbol: &str, session_open: i64, bars: &[Bar], p: &OrbParams) -> Option<Candidate> {
    let (cur, earlier) = bars.split_last()?;
    let in_order = bars.windows(2).all(|w| w[0].ts < w[1].ts);
    if p.validate().is_err()
        || !in_order
        || bars[0].ts < session_open
        || !bars.iter().all(Bar::is_sane)
    {
        return None;
    }
    let or_end = session_open.checked_add(p.or_minutes * 60)?;
    if cur.ts < or_end {
        return None;
    }
    let in_range = earlier.iter().filter(|b| b.ts < or_end);
    if in_range.clone().count() < p.min_or_bars {
        return None;
    }
    let or_high = in_range.clone().map(|b| b.high).reduce(f64::max)?;
    let or_low = in_range.map(|b| b.low).reduce(f64::min)?;
    let broke_out_before = earlier.iter().any(|b| b.ts >= or_end && b.close > or_high);
    if broke_out_before || cur.close <= or_high {
        return None;
    }
    let avg_volume = earlier.iter().map(|b| b.volume as f64).sum::<f64>() / earlier.len() as f64;
    if cur.volume as f64 <= avg_volume {
        return None;
    }
    let atr = atr(bars, p.atr_len)?;

    let entry = Price::from_f64_ceil(cur.close)?;
    let stop = Price::from_f64_floor(or_low.max(cur.close - p.atr_mult * atr))?;
    let r = risk_per_share(entry, stop)?;
    let min_r = ((entry.cents() as f64 * p.min_r_frac).ceil() as i64).max(p.min_r_cents);
    if r.cents() < min_r {
        return None;
    }
    let reward = (r.cents() as f64 * p.target_r).floor() as i64; // finite: params validated
    let target = Price::from_cents(entry.cents().checked_add(reward)?)?;
    let reason = format!(
        "ORB{} long: close {:.2} > OR high {:.2}; volume {} > avg {:.0}; ATR {:.2}",
        p.or_minutes, cur.close, or_high, cur.volume, avg_volume, atr
    );
    Candidate::long(symbol, entry, stop, target, reason).ok()
}

/// Simple average of the last `len` true ranges, using today's bars only (no previous-day close).
/// None until there are `len` true ranges. Callers must pass sane bars (see `Bar::is_sane`).
fn atr(bars: &[Bar], len: usize) -> Option<f64> {
    if len == 0 || bars.len() <= len {
        return None;
    }
    let trs = bars.windows(2).map(|w| {
        let (prev_close, b) = (w[0].close, w[1]);
        (b.high - b.low)
            .max((b.high - prev_close).abs())
            .max((b.low - prev_close).abs())
    });
    let sum: f64 = trs.skip(bars.len() - 1 - len).sum();
    let v = sum / len as f64;
    v.is_finite().then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: i64 = 1_700_000_000; // any minute boundary works

    fn bar(minute: i64, high: f64, low: f64, close: f64, volume: u64) -> Bar {
        Bar {
            ts: OPEN + minute * 60,
            open: (high + low) / 2.0,
            high,
            low,
            close,
            volume,
        }
    }

    /// 15 opening-range bars between 100.00 and 101.00, volume 1000.
    fn opening_range() -> Vec<Bar> {
        (0..15).map(|m| bar(m, 101.0, 100.0, 100.5, 1000)).collect()
    }

    fn cents(c: &Candidate) -> (i64, i64, i64) {
        (c.entry().cents(), c.stop().cents(), c.target().cents())
    }

    fn run(bars: &[Bar]) -> Option<Candidate> {
        orb_long("SPY", OPEN, bars, &OrbParams::default())
    }

    #[test]
    fn breakout_produces_a_valid_candidate() {
        let mut bars = opening_range();
        bars.push(bar(15, 101.6, 100.9, 101.5, 3000));
        let c = run(&bars).expect("should fire");
        // ATR = (13 × 1.00 + 1.10) / 14 ≈ 1.007 → ATR stop 99.99 < OR low 100.00, so stop = 100.00
        assert_eq!(cents(&c), (10_150, 10_000, 10_450));
        assert_eq!(c.r().cents(), 150);
        assert_eq!(c.symbol(), "SPY");
        assert!(c.reason().contains("OR high 101.00"), "{}", c.reason());
    }

    #[test]
    fn atr_stop_is_used_when_closer_than_the_range_low() {
        // wide range (low 95) but calm bars: the ATR stop sits much closer to entry
        let mut bars: Vec<Bar> = (0..15).map(|m| bar(m, 101.0, 100.6, 100.8, 1000)).collect();
        bars[0] = bar(0, 101.0, 95.0, 100.8, 1000);
        bars.push(bar(15, 101.3, 100.9, 101.2, 3000));
        let c = run(&bars).expect("should fire");
        // last 14 TRs = 13 × 0.40 + 0.50 → ATR ≈ 0.4071; stop = 101.20 − 0.6107 = 100.589 → floor 100.58
        assert_eq!(cents(&c), (10_120, 10_058, 10_244));
    }

    #[test]
    fn nothing_during_the_opening_range() {
        let bars: Vec<Bar> = (0..10).map(|m| bar(m, 105.0, 100.0, 104.0, 9000)).collect();
        assert_eq!(run(&bars), None);
    }

    #[test]
    fn needs_a_close_above_the_range_high() {
        let mut bars = opening_range();
        bars.push(bar(15, 101.4, 100.8, 101.0, 3000)); // high pierces, close doesn't clear
        assert_eq!(run(&bars), None);
    }

    #[test]
    fn needs_above_average_volume() {
        let mut bars = opening_range();
        bars.push(bar(15, 101.6, 100.9, 101.5, 1000));
        assert_eq!(run(&bars), None);
    }

    #[test]
    fn fires_only_on_the_first_breakout() {
        let mut bars = opening_range();
        bars.push(bar(15, 101.6, 100.9, 101.5, 3000));
        bars.push(bar(16, 100.9, 100.4, 100.6, 1000)); // falls back inside
        bars.push(bar(17, 101.9, 101.0, 101.8, 5000)); // breaks out again
        assert_eq!(run(&bars), None);
    }

    #[test]
    fn a_low_volume_first_close_above_ends_the_day() {
        let mut bars = opening_range();
        bars.push(bar(15, 101.3, 100.9, 101.2, 500)); // first close above, too little volume
        bars.push(bar(16, 101.9, 101.0, 101.8, 5000)); // stronger, but not the first
        assert_eq!(run(&bars), None);
    }

    #[test]
    fn fires_exactly_once_per_day_on_the_first_breakout_bar() {
        // Look-ahead is ruled out by the signature (only the slice is visible); this checks that
        // replaying a day bar by bar yields exactly one candidate, on the right bar.
        let mut bars = opening_range();
        bars.extend([
            bar(15, 101.0, 100.7, 100.9, 900),
            bar(16, 101.6, 100.9, 101.5, 3000),
            bar(17, 102.0, 101.4, 101.9, 2500),
            bar(18, 102.3, 101.8, 102.2, 2600),
        ]);
        let fired: Vec<usize> = (1..=bars.len())
            .filter(|&n| run(&bars[..n]).is_some())
            .collect();
        assert_eq!(fired, [17], "fires on the 101.5 close (bar index 16)");
    }

    #[test]
    fn rejects_a_stop_too_tight() {
        // 5-cent range on a $100 stock: R ≈ 5¢ < minimum 0.15% ≈ 16¢
        let mut bars: Vec<Bar> = (0..15)
            .map(|m| bar(m, 100.05, 100.00, 100.02, 1000))
            .collect();
        bars.push(bar(15, 100.08, 100.03, 100.06, 3000));
        assert_eq!(run(&bars), None);
    }

    #[test]
    fn five_cent_minimum_r_applies_to_cheap_stocks() {
        // $10 stock: 0.15% is 2¢, so the 5¢ floor decides. R = 5¢ exactly passes…
        let mut ok: Vec<Bar> = (0..15).map(|m| bar(m, 10.04, 10.00, 10.02, 1000)).collect();
        ok.push(bar(15, 10.06, 10.03, 10.05, 3000));
        let c = run(&ok).expect("R equal to the minimum is allowed");
        assert_eq!((c.r().cents(), c.target().cents()), (5, 1015));
        // …R = 4¢ does not
        let mut tight: Vec<Bar> = (0..15).map(|m| bar(m, 10.04, 10.01, 10.02, 1000)).collect();
        tight.push(bar(15, 10.06, 10.03, 10.05, 3000));
        assert_eq!(run(&tight), None);
    }

    #[test]
    fn a_sparse_opening_range_is_ignored() {
        // IEX gaps: one bar in the range, plenty after it — the range itself is meaningless
        let mut bars = vec![bar(0, 101.0, 100.0, 100.5, 1000)];
        bars.extend((15..30).map(|m| bar(m, 101.0, 100.2, 100.6, 1000)));
        bars.push(bar(30, 101.6, 100.9, 101.5, 3000));
        assert_eq!(run(&bars), None);
        // 9 of 15 range bars is still too few; 10 is enough
        let mut nine: Vec<Bar> = opening_range().into_iter().take(9).collect();
        nine.extend((15..21).map(|m| bar(m, 100.9, 100.2, 100.6, 1000)));
        nine.push(bar(21, 101.6, 100.9, 101.5, 3000));
        assert_eq!(run(&nine), None);
        let mut ten: Vec<Bar> = opening_range().into_iter().take(10).collect();
        ten.extend((15..20).map(|m| bar(m, 100.9, 100.2, 100.6, 1000)));
        ten.push(bar(20, 101.6, 100.9, 101.5, 3000));
        assert!(run(&ten).is_some());
    }

    #[test]
    fn needs_enough_bars_for_the_atr() {
        let p = OrbParams {
            atr_len: 30,
            ..OrbParams::default()
        };
        let mut bars = opening_range();
        bars.push(bar(15, 101.6, 100.9, 101.5, 3000));
        assert_eq!(orb_long("SPY", OPEN, &bars, &p), None);
    }

    #[test]
    fn any_bad_bar_means_no_candidate() {
        let breakout = bar(15, 101.6, 100.9, 101.5, 3000);
        let with = |f: &dyn Fn(&mut Vec<Bar>)| {
            let mut bars = opening_range();
            bars.push(breakout);
            f(&mut bars);
            run(&bars)
        };
        assert!(with(&|_| {}).is_some(), "control case fires");
        assert_eq!(with(&|b| b[3].high = f64::NAN), None); // NaN would vanish from max()
        assert_eq!(with(&|b| b[3].close = f64::NAN), None);
        assert_eq!(with(&|b| b[3].high = f64::INFINITY), None);
        assert_eq!(with(&|b| b[3].low = 0.0), None);
        assert_eq!(with(&|b| b[3].low = 102.0), None); // low above high
        assert_eq!(with(&|b| b[15].close = f64::NAN), None);
        assert_eq!(with(&|b| b.swap(2, 3)), None); // out of order
        assert_eq!(with(&|b| b[4].ts = b[3].ts), None); // duplicate timestamp
        assert_eq!(
            with(&|b| b.insert(0, bar(-5, 101.0, 100.0, 100.5, 1000))),
            None
        ); // premarket
    }

    #[test]
    fn invalid_params_are_rejected_loudly_and_never_trade() {
        assert!(OrbParams::default().validate().is_ok());
        let bad = [
            OrbParams {
                or_minutes: 0,
                ..OrbParams::default()
            },
            OrbParams {
                or_minutes: i64::MAX,
                ..OrbParams::default()
            },
            OrbParams {
                min_or_bars: 0,
                ..OrbParams::default()
            },
            OrbParams {
                min_or_bars: 16,
                ..OrbParams::default()
            },
            OrbParams {
                atr_len: 0,
                ..OrbParams::default()
            },
            OrbParams {
                atr_mult: f64::NAN,
                ..OrbParams::default()
            },
            OrbParams {
                atr_mult: -1.0,
                ..OrbParams::default()
            },
            OrbParams {
                target_r: 0.5,
                ..OrbParams::default()
            },
            OrbParams {
                target_r: f64::INFINITY,
                ..OrbParams::default()
            },
            OrbParams {
                min_r_frac: -0.1,
                ..OrbParams::default()
            },
            OrbParams {
                min_r_cents: -1,
                ..OrbParams::default()
            },
        ];
        let mut bars = opening_range();
        bars.push(bar(15, 101.6, 100.9, 101.5, 3000));
        for p in bad {
            assert!(p.validate().is_err(), "{p:?}");
            assert_eq!(orb_long("SPY", OPEN, &bars, &p), None, "{p:?}");
        }
    }

    #[test]
    fn candidate_enforces_stop_entry_target_order() {
        let p = |c| Price::from_cents(c).unwrap();
        let mk = |e, s, t| Candidate::long("X", p(e), p(s), p(t), String::new());
        assert!(mk(100, 99, 102).is_ok());
        let sym = |s| {
            Candidate::long(s, p(100), p(99), p(102), String::new()).map(|c| c.symbol().to_string())
        };
        assert_eq!(sym("brk.b"), Ok("BRK.B".to_string()));
        for bad in ["", " SPY", "SP Y", "SPY;", "TOOLONGSYMBOL"] {
            assert!(sym(bad).is_err(), "{bad:?}");
        }
        assert!(mk(100, 100, 102).is_err());
        assert!(mk(100, 101, 102).is_err());
        assert!(mk(100, 99, 100).is_err());
    }
}
