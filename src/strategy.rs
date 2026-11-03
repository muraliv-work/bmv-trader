//! Strategies are pure functions: today's bars so far → maybe a Candidate. No clock, no I/O, so the
//! backtest and the live engine get identical answers from identical bars (DESIGN §4, §7).
//! Sizing is not done here; `risk.rs` sizes from entry/stop and the limits.

use crate::money::{Price, Usd, risk_per_share};

/// One 1-minute bar. `ts` is the bar's **start** (Unix seconds, UTC), as Alpaca stamps it.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Entry limit this many bps above the signal close: 0 rests at the close (fills only if price
    /// comes back down); > 0 is a marketable limit that fills at once, capped at that price.
    pub entry_buffer_bps: i64,
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
            entry_buffer_bps: 0,
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
            && self.min_r_cents >= 0
            && (0..=50).contains(&self.entry_buffer_bps);
        if ok {
            Ok(())
        } else {
            Err(format!("invalid ORB parameters: {self:?}"))
        }
    }
}

/// Today's direction so far, from regular-hours bars only (DESIGN §7: no premarket, no prior day).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trend {
    Up,
    Down,
    Flat,
}

/// Up: the last close is above both today's VWAP and today's first open; Down: below both.
/// Too few bars, bad bars or no volume mean Flat, which a trend filter treats as "don't trade".
pub fn trend(bars: &[Bar], min_bars: usize) -> Trend {
    let (Some(first), Some(last)) = (bars.first(), bars.last()) else {
        return Trend::Flat;
    };
    if bars.len() < min_bars.max(1) || !bars.iter().all(Bar::is_sane) {
        return Trend::Flat;
    }
    let (pv, vol) = bars.iter().fold((0.0, 0.0), |(pv, vol), b| {
        let v = b.volume as f64;
        (pv + (b.high + b.low + b.close) / 3.0 * v, vol + v)
    });
    if vol <= 0.0 {
        return Trend::Flat;
    }
    let vwap = pv / vol;
    if last.close > vwap && last.close > first.open {
        Trend::Up
    } else if last.close < vwap && last.close < first.open {
        Trend::Down
    } else {
        Trend::Flat
    }
}

/// ORB long (DESIGN §7), evaluated on the **last** bar of `bars` — today's regular-hours bars so far,
/// oldest first. Fires only on the first bar after the opening range that closes above its high
/// (if that bar fails the volume filter, the day is done), with volume above the average of the
/// earlier bars today. Bad bars, out-of-order bars, or bad params mean no candidate.
pub fn orb_long(symbol: &str, session_open: i64, bars: &[Bar], p: &OrbParams) -> Option<Candidate> {
    let v = Today::after_range(session_open, bars, p)?;
    let avg_volume = first_breakout(&v)?;
    let atr = atr(bars, p.atr_len)?;
    let reason = format!(
        "ORB{} long: close {:.2} > OR high {:.2}; volume {} > avg {:.0}; ATR {:.2}",
        p.or_minutes, v.cur.close, v.or_high, v.cur.volume, avg_volume, atr
    );
    long_with_exits(symbol, v.cur.close, v.or_low, atr, p, reason)
}

/// `v.cur` is the first close above the range high, on above-average volume: returns that average.
fn first_breakout(v: &Today) -> Option<f64> {
    let broke_out_before = v
        .earlier
        .iter()
        .any(|b| b.ts >= v.or_end && b.close > v.or_high);
    if broke_out_before || v.cur.close <= v.or_high {
        return None;
    }
    let avg_volume =
        v.earlier.iter().map(|b| b.volume as f64).sum::<f64>() / v.earlier.len() as f64;
    (v.cur.volume as f64 > avg_volume).then_some(avg_volume)
}

/// Gap-and-go (DESIGN §7b, trial 4): only on a day the first bar opens at least `GAP_MIN_FRAC` above
/// yesterday's last regular-hours close. Then the ORB rule over a 5-minute range (at least 3 bars),
/// and the gap still open at the signal (close above yesterday's close). ATR over today's bars so
/// far (up to `atr_len`, at least 4). Same stop rule, target and sizing.
pub fn gap_long(
    symbol: &str,
    session_open: i64,
    bars: &[Bar],
    prev_close: f64,
    p: &OrbParams,
) -> Option<Candidate> {
    const GAP_MIN_FRAC: f64 = 0.01;
    let range = OrbParams {
        or_minutes: 5,
        min_or_bars: 3,
        ..p.clone()
    };
    let v = Today::after_range(session_open, bars, &range)?;
    if !(prev_close.is_finite() && prev_close > 0.0) {
        return None;
    }
    let gap = bars[0].open / prev_close - 1.0;
    if gap < GAP_MIN_FRAC || v.cur.close <= prev_close {
        return None;
    }
    let avg_volume = first_breakout(&v)?;
    // The breakout usually comes before 14 true ranges exist: use what today has, at least 4.
    let atr = atr(bars, p.atr_len.min(bars.len() - 1)).filter(|_| bars.len() > 4)?;
    let reason = format!(
        "gap-and-go long: gap {:.2}% over {prev_close:.2}; close {:.2} > 5-min high {:.2}; volume {} > avg {avg_volume:.0}; ATR {atr:.2}",
        gap * 100.0,
        v.cur.close,
        v.or_high,
        v.cur.volume
    );
    long_with_exits(symbol, v.cur.close, v.or_low, atr, p, reason)
}

/// VWAP pullback (DESIGN §7c, trial 5), on the first bar of the day, from 10:00 on, where:
/// today trends up (close above VWAP and the first open), one of the 3 bars before it touched
/// VWAP (low at or below VWAP then), and it closes above VWAP and the previous bar's high.
/// Stop: the closer of the 3-bar pullback low and close − k × ATR. Same target and sizing as ORB.
pub fn vwap_pullback_long(
    symbol: &str,
    session_open: i64,
    bars: &[Bar],
    p: &OrbParams,
) -> Option<Candidate> {
    const LOOKBACK: usize = 3;
    // 30 minutes before any signal, at least 20 bars of them: VWAP and the trend need history.
    let warmup = OrbParams {
        or_minutes: 30,
        min_or_bars: 20,
        ..p.clone()
    };
    let v = Today::after_range(session_open, bars, &warmup)?;
    let vwap = running_vwap(bars)?;
    let first_open = bars[0].open;
    let signal = |i: usize| {
        let b = &bars[i];
        i >= LOOKBACK
            && b.ts >= v.or_end
            && b.close > vwap[i]
            && b.close > first_open
            && b.close > bars[i - 1].high
            && (i - LOOKBACK..i).any(|j| bars[j].low <= vwap[j])
    };
    let last = bars.len() - 1;
    if !signal(last) || (0..last).any(signal) {
        return None;
    }
    let pullback_low = bars[last - LOOKBACK..last]
        .iter()
        .map(|b| b.low)
        .reduce(f64::min)?;
    let atr = atr(bars, p.atr_len)?;
    let reason = format!(
        "VWAP pullback long: close {:.2} > VWAP {:.2} and prior high {:.2}; pullback low {pullback_low:.2}; ATR {atr:.2}",
        v.cur.close,
        vwap[last],
        bars[last - 1].high
    );
    long_with_exits(symbol, v.cur.close, pullback_low, atr, p, reason)
}

/// VWAP after each bar (typical price × volume); None if no volume has traded by the last bar.
/// Before the first traded volume, VWAP is the bar's typical price.
fn running_vwap(bars: &[Bar]) -> Option<Vec<f64>> {
    let (mut pv, mut vol) = (0.0, 0.0);
    let out: Vec<f64> = bars
        .iter()
        .map(|b| {
            let typical = (b.high + b.low + b.close) / 3.0;
            pv += typical * b.volume as f64;
            vol += b.volume as f64;
            if vol > 0.0 { pv / vol } else { typical }
        })
        .collect();
    (vol > 0.0).then_some(out)
}

/// Random-entry control for the P1 gate (DESIGN §13): the same day, data checks, stop, target and
/// sizing as ORB, but entering at one pseudo-random minute per symbol and day with no signal at all.
/// The minute depends only on (symbol, session, seed), so a replay is repeatable.
pub fn random_long(
    symbol: &str,
    session_open: i64,
    bars: &[Bar],
    p: &OrbParams,
    seed: u64,
) -> Option<Candidate> {
    /// Entries spread over the 5 hours after the opening range: before the 30-minute cutoff on a
    /// full day; on a half-day, late picks are rejected by the engine's cutoff like any late entry.
    const WINDOW_MINUTES: u64 = 300;
    let v = Today::after_range(session_open, bars, p)?;
    let pick = v
        .or_end
        .checked_add(60 * (mix(symbol, session_open, seed) % WINDOW_MINUTES) as i64)?;
    let picked_earlier = v.earlier.iter().any(|b| b.ts >= pick);
    if v.cur.ts < pick || picked_earlier {
        return None;
    }
    let atr = atr(bars, p.atr_len)?;
    let reason = format!(
        "random entry (seed {seed}): close {:.2}; ATR {:.2}",
        v.cur.close, atr
    );
    long_with_exits(symbol, v.cur.close, v.or_low, atr, p, reason)
}

/// Today's bars after the opening range, validated: in order, sane, none before the open,
/// a full enough range, valid params.
struct Today<'a> {
    cur: &'a Bar,
    earlier: &'a [Bar],
    or_end: i64,
    or_high: f64,
    or_low: f64,
}

impl<'a> Today<'a> {
    fn after_range(session_open: i64, bars: &'a [Bar], p: &OrbParams) -> Option<Self> {
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
        Some(Today {
            cur,
            earlier,
            or_end,
            or_high,
            or_low,
        })
    }
}

/// Entry at the close (rounded up), stop at the closer of the range low (if below) and
/// close − k × ATR (rounded down), target at `target_r` × R; refuses a stop that's too tight.
fn long_with_exits(
    symbol: &str,
    close: f64,
    or_low: f64,
    atr: f64,
    p: &OrbParams,
    reason: String,
) -> Option<Candidate> {
    let atr_stop = close - p.atr_mult * atr;
    let stop = if or_low < close {
        or_low.max(atr_stop)
    } else {
        atr_stop
    };
    let entry = Price::from_f64_ceil(close * (1.0 + p.entry_buffer_bps as f64 / 10_000.0))?;
    let stop = Price::from_f64_floor(stop)?;
    let r = risk_per_share(entry, stop)?;
    let min_r = ((entry.cents() as f64 * p.min_r_frac).ceil() as i64).max(p.min_r_cents);
    if r.cents() < min_r {
        return None;
    }
    let reward = (r.cents() as f64 * p.target_r).floor() as i64; // finite: params validated
    let target = Price::from_cents(entry.cents().checked_add(reward)?)?;
    Candidate::long(symbol, entry, stop, target, reason).ok()
}

/// FNV-1a over the inputs, then a splitmix finalizer: stable across platforms and Rust versions.
fn mix(symbol: &str, session_open: i64, seed: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in symbol
        .bytes()
        .chain(session_open.to_le_bytes())
        .chain(seed.to_le_bytes())
    {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
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

    fn gap(bars: &[Bar], prev_close: f64) -> Option<Candidate> {
        gap_long("SPY", OPEN, bars, prev_close, &OrbParams::default())
    }

    /// 5 range bars between 100.00 and 101.00 (first open 100.50), then a breakout at minute 5.
    fn gap_day() -> Vec<Bar> {
        let mut bars: Vec<Bar> = (0..5).map(|m| bar(m, 101.0, 100.0, 100.5, 1000)).collect();
        bars.push(bar(5, 101.6, 100.9, 101.5, 3000));
        bars
    }

    #[test]
    fn gap_and_go_needs_a_gap_of_one_percent_still_open() {
        let c = gap(&gap_day(), 99.0).expect("1.5% gap and a breakout should fire");
        // ATR(4) = (3 × 1.00 + 1.10) / 4 = 1.025 → ATR stop 99.96 < range low 100.00
        assert_eq!(cents(&c), (10_150, 10_000, 10_450));
        assert!(c.reason().contains("gap 1.52%"), "{}", c.reason());
        assert_eq!(gap(&gap_day(), 100.0), None, "0.5% is no gap");
        assert_eq!(gap(&gap_day(), 101.6), None, "gap down / filled");
        assert_eq!(gap(&gap_day(), f64::NAN), None);
        assert_eq!(gap(&gap_day(), 0.0), None);
        assert_eq!(
            gap(&gap_day()[..5], 99.0),
            None,
            "no signal inside the range"
        );
        let mut late = gap_day();
        late.insert(5, bar(5, 101.3, 100.9, 101.2, 3000)); // an earlier breakout took the day
        for (m, b) in late.iter_mut().enumerate() {
            b.ts = OPEN + m as i64 * 60;
        }
        assert_eq!(gap(&late, 99.0), None);
    }

    fn vwap(bars: &[Bar]) -> Option<Candidate> {
        vwap_pullback_long("SPY", OPEN, bars, &OrbParams::default())
    }

    /// 30 flat bars around 100.00 (each touches VWAP), then an up bar at `minute`.
    fn vwap_day(minute: i64) -> Vec<Bar> {
        let mut bars: Vec<Bar> = (0..minute)
            .map(|m| bar(m, 100.5, 99.5, 100.0, 1000))
            .collect();
        bars.push(bar(minute, 101.0, 100.2, 100.9, 1000));
        bars
    }

    #[test]
    fn vwap_pullback_fires_on_the_first_resumption_after_ten() {
        let c = vwap(&vwap_day(30)).expect("resumption above VWAP after a touch");
        // pullback low 99.50 vs ATR stop 100.90 − 1.5 × 1.00 = 99.40: the closer is 99.50
        assert_eq!(cents(&c), (10_090, 9_950, 10_370));
        assert!(c.reason().starts_with("VWAP pullback"), "{}", c.reason());
        assert_eq!(vwap(&vwap_day(20)), None, "not before 10:00");
        let mut second = vwap_day(30);
        second.push(bar(31, 100.8, 99.9, 100.3, 1000));
        second.push(bar(32, 101.4, 100.4, 101.3, 1000));
        assert_eq!(vwap(&second), None, "only the first signal of the day");
        let mut down = vwap_day(30);
        let last = down.last_mut().unwrap();
        (last.high, last.low, last.close) = (100.6, 99.0, 99.2);
        assert_eq!(vwap(&down), None, "below VWAP and the first open");
        let mut no_touch: Vec<Bar> = (0..30)
            .map(|m| {
                let p = 100.0 + m as f64 * 0.5; // a steady climb never comes back to VWAP
                bar(m, p + 0.2, p - 0.1, p + 0.1, 1000)
            })
            .collect();
        no_touch.push(bar(30, 115.5, 115.1, 115.4, 1000));
        assert_eq!(vwap(&no_touch), None, "no pullback, no trade");
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

    fn quiet_day() -> Vec<Bar> {
        let mut bars = opening_range();
        bars.extend((15..330).map(|m| bar(m, 100.9, 100.4, 100.6, 1000)));
        bars
    }

    fn random_fires(bars: &[Bar], seed: u64) -> Vec<(usize, Candidate)> {
        (1..=bars.len())
            .filter_map(|n| {
                random_long("SPY", OPEN, &bars[..n], &OrbParams::default(), seed)
                    .map(|c| (n - 1, c))
            })
            .collect()
    }

    #[test]
    fn the_random_baseline_fires_once_without_any_signal() {
        let bars = quiet_day(); // never breaks the range: ORB never fires
        assert!((1..=bars.len()).all(|n| run(&bars[..n]).is_none()));
        let fired = random_fires(&bars, 7);
        assert_eq!(fired.len(), 1);
        let (i, c) = &fired[0];
        assert!(bars[*i].ts >= OPEN + 15 * 60, "after the opening range");
        // same exit construction as ORB: stop at the range low (closer than the ATR stop), 2R target
        assert_eq!((c.entry().cents(), c.stop().cents()), (10_060, 10_000));
        assert_eq!(c.target().cents(), 10_060 + 2 * 60);
    }

    #[test]
    fn the_random_baseline_is_repeatable_and_seeded() {
        let bars = quiet_day();
        assert_eq!(random_fires(&bars, 7), random_fires(&bars, 7));
        let minutes: std::collections::BTreeSet<usize> = (1..20)
            .filter_map(|s| random_fires(&bars, s).first().map(|(i, _)| *i))
            .collect();
        assert!(
            minutes.len() > 10,
            "different seeds pick different minutes: {minutes:?}"
        );
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

    fn tb(open: f64, close: f64, volume: u64) -> Bar {
        Bar {
            ts: 0,
            open,
            high: open.max(close) + 0.1,
            low: open.min(close) - 0.1,
            close,
            volume,
        }
    }

    #[test]
    fn trend_follows_price_versus_vwap_and_the_open() {
        let rising: Vec<Bar> = (0..20)
            .map(|i| tb(100.0 + i as f64 * 0.1, 100.1 + i as f64 * 0.1, 100))
            .collect();
        let falling: Vec<Bar> = (0..20)
            .map(|i| tb(100.0 - i as f64 * 0.1, 99.9 - i as f64 * 0.1, 100))
            .collect();
        assert_eq!(trend(&rising, 10), Trend::Up);
        assert_eq!(trend(&falling, 10), Trend::Down);
        // back at the open after a rally: above VWAP? no, below; not below the open either → Flat
        let mut round_trip = rising.clone();
        round_trip.push(tb(101.9, 100.0, 100));
        assert_eq!(trend(&round_trip, 10), Trend::Flat);
    }

    #[test]
    fn trend_is_flat_when_it_cannot_tell() {
        let rising: Vec<Bar> = (0..20)
            .map(|i| tb(100.0 + i as f64 * 0.1, 100.1 + i as f64 * 0.1, 100))
            .collect();
        assert_eq!(trend(&[], 10), Trend::Flat);
        assert_eq!(trend(&rising[..5], 10), Trend::Flat, "too few bars");
        let silent: Vec<Bar> = rising.iter().map(|b| Bar { volume: 0, ..*b }).collect();
        assert_eq!(trend(&silent, 10), Trend::Flat, "no volume");
        let mut bad = rising.clone();
        bad[3].close = f64::NAN;
        assert_eq!(trend(&bad, 10), Trend::Flat, "bad bar");
    }

    #[test]
    fn entry_buffer_raises_only_the_entry_and_keeps_the_target_at_target_r() {
        let mut bars = opening_range();
        bars.push(bar(15, 101.6, 100.9, 101.5, 3000));
        let at_close = orb_long("SPY", OPEN, &bars, &OrbParams::default()).unwrap();
        let p = OrbParams {
            entry_buffer_bps: 5,
            ..OrbParams::default()
        };
        let marketable = orb_long("SPY", OPEN, &bars, &p).unwrap();
        // 101.50 × 1.0005 = 101.55075 → ceil to 101.56; same stop; target = entry + 2R
        assert_eq!(marketable.entry().cents(), 10_156);
        assert_eq!(marketable.stop(), at_close.stop());
        let r = marketable.entry().cents() - marketable.stop().cents();
        assert_eq!(
            marketable.target().cents(),
            marketable.entry().cents() + 2 * r
        );
        for bad in [-1, 51] {
            assert!(
                OrbParams {
                    entry_buffer_bps: bad,
                    ..OrbParams::default()
                }
                .validate()
                .is_err()
            );
        }
    }
}
