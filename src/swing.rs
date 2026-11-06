//! Trial 8: the Desk Signal swing strategy on adjusted daily bars (DESIGN §7d). Long-only, held
//! 2–10 sessions, at most 5 open. Research only: the engine has no swing book yet.

use crate::momentum::{self, Calendar};
use crate::scan::DailyBar;
use crate::stats::{self, GateCheck, GateReport};
use std::collections::BTreeMap;

/// The pre-registered rules (DESIGN §7d). Changing any of these is a new trial.
pub const EMAS: [usize; 4] = [8, 21, 50, 200];
pub const MIN_HISTORY: usize = 250;
pub const BREAKOUT: usize = 20;
pub const VOLUME_RATIO: f64 = 1.5;
pub const RSI_LEN: usize = 14;
pub const RSI_MIN: f64 = 50.0;
pub const ATR_LEN: usize = 14;
pub const ATR_WINDOW: usize = 100;
pub const ATR_MAX_PERCENTILE: f64 = 0.9;
pub const MAX_CHASE: f64 = 0.005;
pub const MAX_OPEN: usize = 5;
pub const STOP_ATR: f64 = 2.0;
pub const TARGET_ATR: f64 = 3.0;
pub const MAX_HOLD: usize = 10;
pub const COST_PER_SIDE: f64 = 0.002;
pub const RANDOM_SEEDS: u64 = 5;
/// R converts to equity at this risk per trade, to compare with SPY.
pub const RISK_PER_TRADE: f64 = 0.01;
pub const FOLD_SESSIONS: usize = 63;
pub const RESEARCH_FROM: &str = "2017-01-03";
/// What differs between the swing trials (DESIGN §7d, §7e); everything else is shared.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rules {
    pub trial: u32,
    pub name: &'static str,
    /// ATR(14) at the signal close must be at least this fraction of the close (0 = none).
    pub min_atr_fraction: f64,
    /// Leave out funds by Alpaca asset name.
    pub stocks_only: bool,
    /// The time stop: sold at the open of session entry + max_hold.
    pub max_hold: usize,
    /// The profit target in ATRs; none in trial 13 (§7j).
    pub target_atr: Option<f64>,
    pub signal: Source,
}

/// What makes a day a signal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    /// The Desk Signal (§7d), ranked by volume ratio.
    DeskSignal,
    /// Insider cluster buys (§7j), ranked by the cluster's dollars.
    Insider,
}

pub const TRIAL_8: Rules = Rules {
    trial: 8,
    name: "swing desk-signal",
    min_atr_fraction: 0.0,
    stocks_only: false,
    max_hold: MAX_HOLD,
    target_atr: Some(TARGET_ATR),
    signal: Source::DeskSignal,
};
pub const TRIAL_9: Rules = Rules {
    trial: 9,
    name: "swing desk-signal vol-stocks",
    min_atr_fraction: 0.015,
    stocks_only: true,
    max_hold: MAX_HOLD,
    target_atr: Some(TARGET_ATR),
    signal: Source::DeskSignal,
};
/// Trial 9's filters, stop and costs on insider cluster buys; no target, 20 sessions (§7j).
pub const TRIAL_13: Rules = Rules {
    trial: 13,
    name: "swing insider-cluster",
    max_hold: 20,
    target_atr: None,
    signal: Source::Insider,
    ..TRIAL_9
};

/// A fund by its Alpaca asset name (DESIGN §7e); case-insensitive.
pub fn is_fund(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let words: Vec<&str> = n
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .collect();
    ["etf", "etn", "etns", "fund", "spdr"]
        .iter()
        .any(|w| words.contains(w))
        || [
            "ishares",
            "proshares",
            "direxion",
            "exchange-traded",
            "exchange traded",
        ]
        .iter()
        .any(|p| n.contains(p))
}

/// Exponential moving average seeded with the simple average of the first `n` values.
fn ema(x: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; x.len()];
    if x.len() < n || n == 0 {
        return out;
    }
    let a = 2.0 / (n as f64 + 1.0);
    let mut e = x[..n].iter().sum::<f64>() / n as f64;
    out[n - 1] = Some(e);
    for i in n..x.len() {
        e += a * (x[i] - e);
        out[i] = Some(e);
    }
    out
}

/// Wilder's smoothing: the first value is the mean of `v[1..=n]`, then (prev·(n−1) + v) / n.
/// `v[0]` is ignored (it has no previous close).
fn wilder(v: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; v.len()];
    if v.len() <= n || n == 0 {
        return out;
    }
    let mut m = v[1..=n].iter().sum::<f64>() / n as f64;
    out[n] = Some(m);
    for i in n + 1..v.len() {
        m = (m * (n as f64 - 1.0) + v[i]) / n as f64;
        out[i] = Some(m);
    }
    out
}

/// Wilder RSI of closes; 100 when there were no losses.
fn rsi(close: &[f64], n: usize) -> Vec<Option<f64>> {
    let change = |i: usize| if i == 0 { 0.0 } else { close[i] - close[i - 1] };
    let gains: Vec<f64> = (0..close.len()).map(|i| change(i).max(0.0)).collect();
    let losses: Vec<f64> = (0..close.len()).map(|i| (-change(i)).max(0.0)).collect();
    wilder(&gains, n)
        .into_iter()
        .zip(wilder(&losses, n))
        .map(|(g, l)| {
            let (g, l) = (g?, l?);
            Some(if l == 0.0 {
                100.0
            } else {
                100.0 - 100.0 / (1.0 + g / l)
            })
        })
        .collect()
}

/// Wilder ATR over the true range.
fn atr(bars: &[&DailyBar], n: usize) -> Vec<Option<f64>> {
    let tr: Vec<f64> = (0..bars.len())
        .map(|i| {
            let b = bars[i];
            match i.checked_sub(1).map(|p| bars[p].close) {
                Some(pc) => (b.high - b.low)
                    .max((b.high - pc).abs())
                    .max((b.low - pc).abs()),
                None => b.high - b.low,
            }
        })
        .collect();
    wilder(&tr, n)
}

/// A trade's result if entered at the open of session `entry`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome {
    pub entry: usize,
    pub exit: usize,
    /// After costs, in R (entry − stop).
    pub r: f64,
    /// (entry − stop) / entry.
    pub stop_fraction: f64,
    /// Sold at the last close because no later bar exists: delisted, or the data simply ends.
    pub at_data_end: bool,
}

/// One symbol on one signal day: always eligible (liquid, with a tradable entry), and a Desk
/// Signal when `ratio` is set (its volume ratio, used to rank).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cand {
    pub symbol: usize,
    pub ratio: Option<f64>,
    pub out: Outcome,
}

/// Entered at the open of `e` with stop and target from `atr`: the first of stop, target or
/// the time stop. Stop first on a day touching both; gaps fill at the open; a symbol with no
/// later bar exits at its last close.
fn simulate_trade(on: &[Option<&DailyBar>], e: usize, atr: f64, rules: &Rules) -> Option<Outcome> {
    let entry = on[e]?.open;
    let stop = entry - STOP_ATR * atr;
    let target = rules.target_atr.map_or(f64::INFINITY, |t| entry + t * atr);
    if !(stop > 0.0 && atr > 0.0) {
        return None;
    }
    let r = |exit: f64, at: usize| Outcome {
        entry: e,
        exit: at,
        r: (exit - entry - COST_PER_SIDE * (entry + exit)) / (entry - stop),
        stop_fraction: (entry - stop) / entry,
        at_data_end: false,
    };
    let max_hold = rules.max_hold;
    let last = (e + max_hold).min(on.len() - 1);
    for (j, b) in on.iter().enumerate().take(last + 1).skip(e) {
        let Some(b) = b else { continue };
        if j == e + max_hold {
            return Some(r(b.open, j));
        }
        if j > e && b.open <= stop {
            return Some(r(b.open, j));
        }
        if b.low <= stop {
            return Some(r(stop, j));
        }
        if j > e && b.open > target {
            return Some(r(b.open, j));
        }
        if b.high > target {
            return Some(r(target, j));
        }
    }
    // no bar at the time stop: the next bar's open, else (delisted) the last close
    if let Some((j, b)) = on
        .iter()
        .enumerate()
        .skip(last + 1)
        .find_map(|(j, b)| Some((j, (*b)?)))
    {
        return Some(r(b.open, j));
    }
    let (j, b) = on[e..=last]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(j, b)| Some((e + j, (*b)?)))?;
    Some(Outcome {
        at_data_end: true,
        ..r(b.close, j)
    })
}

/// Finite, positive, and the high and low bracket the open and close.
fn sane(b: &DailyBar) -> bool {
    [b.open, b.high, b.low, b.close]
        .iter()
        .all(|x| x.is_finite())
        && b.low > 0.0
        && b.low <= b.open.min(b.close)
        && b.high >= b.open.max(b.close)
}

/// A symbol's sane bars on the calendar and its indicators, by own-bar index.
struct Series<'a> {
    on: Vec<Option<&'a DailyBar>>,
    /// Session index of each own bar.
    idx: Vec<usize>,
    own: Vec<&'a DailyBar>,
    emas: Vec<Vec<Option<f64>>>,
    rsi: Vec<Option<f64>>,
    atr: Vec<Option<f64>>,
}

impl<'a> Series<'a> {
    fn new(bars: &'a [DailyBar], cal: &Calendar) -> Self {
        // a bar with an impossible high or low counts as missing: a bad print could fake a fill
        let on: Vec<Option<&DailyBar>> = cal
            .aligned(bars)
            .into_iter()
            .map(|b| b.filter(|b| sane(b)))
            .collect();
        let idx: Vec<usize> = (0..on.len()).filter(|&i| on[i].is_some()).collect();
        let own: Vec<&DailyBar> = idx.iter().map(|&i| on[i].unwrap()).collect();
        let close: Vec<f64> = own.iter().map(|b| b.close).collect();
        Series {
            emas: EMAS.iter().map(|&n| ema(&close, n)).collect(),
            rsi: rsi(&close, RSI_LEN),
            atr: atr(&own, ATR_LEN),
            on,
            idx,
            own,
        }
    }

    /// The ATR at own bar `k` if the symbol is in the trial universe there (history, liquidity,
    /// volatility floor). The baseline draws from exactly this set.
    fn eligible(&self, k: usize, rules: &Rules) -> Option<f64> {
        if k + 1 < MIN_HISTORY || k < ATR_LEN + ATR_WINDOW {
            return None;
        }
        let a = self.atr[k]?;
        (momentum::liquid(&self.on, self.idx[k]) && a >= rules.min_atr_fraction * self.own[k].close)
            .then_some(a)
    }

    fn signal(&self, k: usize) -> Option<f64> {
        desk_signal(k, &self.own, &self.emas, &self.rsi, &self.atr)
    }
}

/// Every eligible day of one symbol, keyed by signal session.
/// For `Source::Insider`, `events` holds the symbol's cluster signals (session → dollars).
pub fn candidates(
    symbol: usize,
    bars: &[DailyBar],
    cal: &Calendar,
    rules: &Rules,
    events: &BTreeMap<usize, f64>,
) -> Vec<(usize, Cand)> {
    let ser = Series::new(bars, cal);
    let mut out = Vec::new();
    for (k, &s) in ser.idx.iter().enumerate() {
        let Some(a) = ser.eligible(k, rules) else {
            continue;
        };
        let Some(next) = ser.on.get(s + 1).copied().flatten() else {
            continue;
        };
        if next.open > ser.own[k].close * (1.0 + MAX_CHASE) {
            continue;
        }
        let Some(trade) = simulate_trade(&ser.on, s + 1, a, rules) else {
            continue;
        };
        let ratio = match rules.signal {
            Source::DeskSignal => ser.signal(k),
            Source::Insider => events.get(&s).copied(),
        };
        out.push((
            s,
            Cand {
                symbol,
                ratio,
                out: trade,
            },
        ));
    }
    out
}

/// A Desk Signal on the calendar's last session (trial 10, DESIGN §7f).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Signal {
    pub symbol: String,
    pub date: String,
    pub ratio: f64,
    pub close: f64,
    pub atr: f64,
}

/// The Desk Signal on the last calendar session, if the symbol traded that day and fired.
pub fn signal_today(
    symbol: &str,
    bars: &[DailyBar],
    cal: &Calendar,
    rules: &Rules,
) -> Option<Signal> {
    let ser = Series::new(bars, cal);
    let k = ser.own.len().checked_sub(1)?;
    if ser.idx[k] + 1 != cal.sessions.len() {
        return None;
    }
    let atr = ser.eligible(k, rules)?;
    Some(Signal {
        symbol: symbol.to_string(),
        date: cal.sessions[ser.idx[k]].clone(),
        ratio: ser.signal(k)?,
        close: ser.own[k].close,
        atr,
    })
}

/// What a forward signal came to, from bars after its day.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Scored {
    /// Not enough later bars yet.
    Pending,
    /// No entry: no bar the next session, or the open chased more than 0.5%.
    Skipped(String),
    Done {
        r: f64,
        exit_date: String,
    },
}

/// A symbol with no bar for this many sessions before the newest is treated as delisted.
pub const DELISTED_AFTER: usize = 5;

/// Scores a logged signal with trial 9's exits from bars after its day. It uses the ATR logged at
/// signal time, rescaled by today's adjustment of the signal-day close, so later downloads can't
/// change whether or how it was eligible. An exit at the last close counts only when the symbol
/// has stopped trading (no bar for `DELISTED_AFTER` sessions); otherwise the trade is still open.
pub fn score_forward(bars: &[DailyBar], cal: &Calendar, signal: &Signal) -> Scored {
    let ser = Series::new(bars, cal);
    let Some(s) = cal.sessions.iter().position(|d| *d == signal.date) else {
        return Scored::Skipped(format!("{} is not in the calendar", signal.date));
    };
    let Some(k) = ser.idx.iter().position(|&i| i == s) else {
        return Scored::Skipped(format!("no bar on {} in the fresh bars", signal.date));
    };
    let scale = ser.own[k].close / signal.close;
    let atr = signal.atr * scale;
    if !(scale.is_finite() && scale > 0.0 && atr.is_finite() && atr > 0.0) {
        return Scored::Skipped("signal close or ATR unusable".into());
    }
    let (e, last) = (s + 1, cal.sessions.len() - 1);
    if e > last {
        return Scored::Pending;
    }
    let Some(next) = ser.on[e] else {
        // a missing bar on the newest session may just be late
        return if e == last {
            Scored::Pending
        } else {
            Scored::Skipped("no bar the next session".into())
        };
    };
    if next.open > ser.own[k].close * (1.0 + MAX_CHASE) {
        return Scored::Skipped("chased: opened > 0.5% above the close".into());
    }
    let newest_bar = ser.idx.last().copied().unwrap_or(0);
    match simulate_trade(&ser.on, e, atr, &TRIAL_9) {
        Some(o) if o.at_data_end && newest_bar + DELISTED_AFTER > last => Scored::Pending,
        Some(o) => Scored::Done {
            r: o.r,
            exit_date: cal.sessions[o.exit].clone(),
        },
        None => Scored::Skipped("no valid stop".into()),
    }
}

/// The Desk Signal at own-bar `k`: its volume ratio, if every condition holds.
fn desk_signal(
    k: usize,
    own: &[&DailyBar],
    emas: &[Vec<Option<f64>>],
    rsi: &[Option<f64>],
    atr: &[Option<f64>],
) -> Option<f64> {
    if k + 1 < MIN_HISTORY || k < ATR_LEN + ATR_WINDOW {
        return None;
    }
    let e: Vec<f64> = emas.iter().map(|v| v[k]).collect::<Option<_>>()?;
    let stacked = e.windows(2).all(|w| w[0] > w[1]);
    let prior = &own[k - BREAKOUT..k];
    let high = prior.iter().map(|b| b.high).fold(f64::MIN, f64::max);
    let avg_volume = prior.iter().map(|b| b.volume as f64).sum::<f64>() / BREAKOUT as f64;
    let ratio = own[k].volume as f64 / avg_volume;
    let mut recent: Vec<f64> = atr[k + 1 - ATR_WINDOW..=k]
        .iter()
        .copied()
        .collect::<Option<_>>()?;
    recent.sort_by(f64::total_cmp);
    let p90 = recent[((ATR_WINDOW - 1) as f64 * ATR_MAX_PERCENTILE) as usize];
    (stacked
        && own[k].close > high
        && avg_volume > 0.0
        && ratio >= VOLUME_RATIO
        && rsi[k]? > RSI_MIN
        && atr[k]? < p90)
        .then_some(ratio)
}

/// A trade taken by the portfolio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Taken {
    pub symbol: usize,
    pub out: Outcome,
}

/// The Desk Signal portfolio: each day, the best signals by volume ratio (then symbol), while
/// fewer than 5 are open and the symbol isn't held. `days[s]` holds signal session `s`'s
/// candidates. Entries before `from` or on/after `cut` are skipped; trades still open at `cut`
/// hold a slot but aren't returned. Also returns how many entries each day took.
pub fn portfolio(
    days: &[Vec<Cand>],
    names: &[String],
    from: usize,
    cut: usize,
) -> (Vec<Taken>, Vec<usize>) {
    let mut open: Vec<Taken> = Vec::new();
    let (mut taken, mut per_day) = (Vec::new(), vec![0; days.len()]);
    for (s, day) in days.iter().enumerate() {
        let e = s + 1;
        if e < from || e >= cut {
            continue;
        }
        // a slot frees only once its exit day has passed
        open.retain(|t| t.out.exit >= e);
        let mut signals: Vec<&Cand> = day.iter().filter(|c| c.ratio.is_some()).collect();
        signals.sort_by(|a, b| {
            b.ratio
                .unwrap()
                .total_cmp(&a.ratio.unwrap())
                .then_with(|| names[a.symbol].cmp(&names[b.symbol]))
        });
        for c in signals {
            if open.len() >= MAX_OPEN {
                break;
            }
            if open.iter().any(|t| t.symbol == c.symbol) {
                continue;
            }
            let t = Taken {
                symbol: c.symbol,
                out: c.out,
            };
            open.push(t);
            per_day[s] += 1;
            if t.out.exit < cut {
                taken.push(t);
            }
        }
    }
    (taken, per_day)
}

/// Random entries: on each day the strategy entered `n` trades, `n` distinct eligible names
/// at random, with the same exits. Pooled over `RANDOM_SEEDS` seeds.
pub fn baseline(days: &[Vec<Cand>], per_day: &[usize], cut: usize) -> Vec<Taken> {
    let mut out = Vec::new();
    for seed in 1..=RANDOM_SEEDS {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        for (day, &n) in days.iter().zip(per_day) {
            let mut idx: Vec<usize> = (0..day.len()).collect();
            for i in 0..n.min(idx.len()) {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let j = i + (rng % (idx.len() - i) as u64) as usize;
                idx.swap(i, j);
                let c = day[idx[i]];
                if c.out.exit < cut {
                    out.push(Taken {
                        symbol: c.symbol,
                        out: c.out,
                    });
                }
            }
        }
    }
    out
}

fn mean_r(t: &[Taken]) -> f64 {
    t.iter().map(|t| t.out.r).sum::<f64>() / t.len().max(1) as f64
}

/// The research result: trades, the baseline, and monthly returns beside SPY's.
pub struct Study {
    pub trades: Vec<Taken>,
    pub baseline: Vec<Taken>,
    /// Research sessions [from, cut): trades are grouped by entry session for the bootstrap.
    pub sessions: std::ops::Range<usize>,
    pub months: Vec<String>,
    pub monthly: Vec<f64>,
    pub spy_monthly: Vec<f64>,
}

impl Study {
    pub fn new(
        trades: Vec<Taken>,
        baseline: Vec<Taken>,
        sessions: std::ops::Range<usize>,
        cal: &Calendar,
        spy: &[DailyBar],
    ) -> Result<Self, String> {
        let month = |i: usize| cal.sessions[i][..7].to_string();
        let months: Vec<String> = {
            let mut m: Vec<String> = sessions.clone().map(month).collect();
            m.dedup();
            m
        };
        let mut by_month: BTreeMap<String, f64> = months.iter().map(|m| (m.clone(), 0.0)).collect();
        for t in &trades {
            *by_month.entry(month(t.out.exit)).or_default() += RISK_PER_TRADE * t.out.r;
        }
        // SPY month-end closes, from the last close before the first research month
        let spy_on = cal.aligned(spy);
        let last_close = |m: &str| {
            // never past the last research session: SPY's last month ends where research does
            (0..sessions.end)
                .rev()
                .find(|&i| cal.sessions[i][..7] <= *m && spy_on[i].is_some())
                .and_then(|i| spy_on[i].map(|b| b.close))
        };
        let before = cal
            .sessions
            .get(sessions.start.wrapping_sub(1))
            .ok_or("no SPY bar before the research period")?;
        let mut prev = spy_on[sessions.start - 1]
            .map(|b| b.close)
            .ok_or(format!("SPY has no bar on {before}"))?;
        let mut spy_monthly = Vec::new();
        for m in &months {
            let c = last_close(m).ok_or(format!("SPY has no bar in {m}"))?;
            spy_monthly.push(c / prev - 1.0);
            prev = c;
        }
        Ok(Study {
            monthly: months.iter().map(|m| by_month[m]).collect(),
            trades,
            baseline,
            sessions,
            months,
            spy_monthly,
        })
    }

    /// The most notional open on any session, as a multiple of equity, at 1% risk per trade
    /// (notional = 1% / stop distance). Equity conversion is uncapped, so this says how much
    /// leverage the CAGR comparison assumes.
    pub fn peak_gross(&self) -> f64 {
        let mut by_session = vec![0.0; self.sessions.end];
        for t in &self.trades {
            let g = RISK_PER_TRADE / t.out.stop_fraction;
            for x in by_session.iter_mut().take(t.out.exit).skip(t.out.entry) {
                *x += g;
            }
        }
        by_session.into_iter().fold(0.0, f64::max)
    }

    fn r_by_session(&self) -> Vec<Vec<f64>> {
        let mut days = vec![Vec::new(); self.sessions.len()];
        for t in &self.trades {
            if let Some(d) = days.get_mut(t.out.entry.wrapping_sub(self.sessions.start)) {
                d.push(t.out.r);
            }
        }
        days
    }

    pub fn expectancy(&self) -> f64 {
        mean_r(&self.trades)
    }

    /// Sum of winning R over the sum of losing R; 0 with no winners, None with no losers.
    pub fn profit_factor(&self) -> Option<f64> {
        let win: f64 = self.trades.iter().map(|t| t.out.r.max(0.0)).sum();
        let loss: f64 = self.trades.iter().map(|t| (-t.out.r).max(0.0)).sum();
        (loss > 0.0).then(|| win / loss)
    }

    /// Worst fall of cumulative R, trades in exit order.
    pub fn max_drawdown_r(&self) -> f64 {
        let mut t = self.trades.clone();
        t.sort_by_key(|t| (t.out.exit, t.out.entry, t.symbol));
        let (mut cum, mut peak, mut worst) = (0.0f64, 0.0f64, 0.0f64);
        for x in &t {
            cum += x.out.r;
            peak = peak.max(cum);
            worst = worst.max(peak - cum);
        }
        worst
    }

    pub fn ci(&self, trials: usize) -> Option<(f64, f64)> {
        let alpha = stats::FAMILY_ALPHA / trials.max(1) as f64;
        let iters = stats::resamples_for(alpha);
        (iters <= stats::MAX_RESAMPLES)
            .then(|| stats::bootstrap_expectancy(&self.r_by_session(), iters, alpha, 42))
            .flatten()
    }

    /// The §7d gate; `trials` includes this one.
    pub fn gate(&self, trials: usize) -> GateReport {
        let alpha = stats::FAMILY_ALPHA / trials.max(1) as f64;
        let ci = self.ci(trials);
        let ci_detail = match ci {
            Some((lo, hi)) => format!(
                "{lo:.3}R .. {hi:.3}R at {:.3}% two-sided ({trials} trials)",
                100.0 * (1.0 - alpha)
            ),
            None => "no interval".into(),
        };
        let base = mean_r(&self.baseline);
        let folds: Vec<std::ops::Range<usize>> = self
            .sessions
            .clone()
            .step_by(FOLD_SESSIONS)
            .map(|s| s..(s + FOLD_SESSIONS).min(self.sessions.end))
            .collect();
        let positive = folds
            .iter()
            .filter(|f| {
                let r: Vec<f64> = self
                    .trades
                    .iter()
                    .filter(|t| f.contains(&t.out.entry))
                    .map(|t| t.out.r)
                    .collect();
                !r.is_empty() && r.iter().sum::<f64>() > 0.0
            })
            .count();
        let pf = self.profit_factor();
        let dd = self.max_drawdown_r();
        let (cagr, spy_cagr) = (
            momentum::cagr(&self.monthly),
            momentum::cagr(&self.spy_monthly),
        );
        let (sh, spy_sh) = (
            momentum::sharpe(&self.monthly),
            momentum::sharpe(&self.spy_monthly),
        );
        let check = |name: &'static str, passed: bool, detail: String| GateCheck {
            name,
            passed,
            detail,
        };
        GateReport {
            checks: vec![
                check(
                    "lower confidence bound of expectancy > 0",
                    ci.is_some_and(|(lo, _)| lo > 0.0),
                    ci_detail.clone(),
                ),
                check(
                    "lower confidence bound beats random entries",
                    !self.baseline.is_empty() && ci.is_some_and(|(lo, _)| lo > base),
                    format!("baseline {base:.3}R over {} trades", self.baseline.len()),
                ),
                check(
                    "most 63-session windows profitable",
                    !folds.is_empty()
                        && positive as f64 >= stats::MIN_POSITIVE_FOLDS * folds.len() as f64,
                    format!("{positive} of {}", folds.len()),
                ),
                check(
                    "profit factor > 1.2",
                    pf.is_none_or(|p| p > 1.2) && !self.trades.is_empty(),
                    pf.map_or("no losing trade".into(), |p| format!("{p:.2}")),
                ),
                check("max drawdown < 10R", dd < 10.0, format!("{dd:.1}R")),
                check(
                    "enough trades to judge",
                    self.trades.len() >= stats::MIN_TRADES,
                    format!("{} (need ≥ {})", self.trades.len(), stats::MIN_TRADES),
                ),
                check(
                    "CAGR above SPY (1% risk per trade)",
                    cagr > spy_cagr,
                    format!("{:+.1}% vs {:+.1}%", cagr * 100.0, spy_cagr * 100.0),
                ),
                check(
                    "Sharpe above SPY",
                    sh > spy_sh,
                    format!("{sh:.2} vs {spy_sh:.2}"),
                ),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(date: &str, o: f64, h: f64, l: f64, c: f64, v: u64) -> DailyBar {
        DailyBar {
            date: date.into(),
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

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

    fn cal(n: usize) -> Calendar {
        let ds = dates(n);
        Calendar::new(
            &ds.iter()
                .map(|d| bar(d, 1.0, 1.0, 1.0, 1.0, 1))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn indicators() {
        let flat = vec![10.0; 30];
        assert_eq!(ema(&flat, 8)[29], Some(10.0));
        assert_eq!(ema(&flat, 8)[6], None);
        let rising: Vec<f64> = (0..30).map(|i| i as f64).collect();
        assert_eq!(rsi(&rising, 14)[29], Some(100.0));
        assert_eq!(rsi(&rising, 14)[13], None);
        let falling: Vec<f64> = (0..30).map(|i| 30.0 - i as f64).collect();
        assert_eq!(rsi(&falling, 14)[29], Some(0.0));
        let ds = dates(30);
        let bars: Vec<DailyBar> = ds
            .iter()
            .map(|d| bar(d, 10.0, 11.0, 9.0, 10.0, 1))
            .collect();
        let refs: Vec<&DailyBar> = bars.iter().collect();
        assert!((atr(&refs, 14)[29].unwrap() - 2.0).abs() < 1e-12);
    }

    /// Bars from `e` on, all at 100 unless overridden; ATR 1 gives stop 98 and target 103.
    fn path(days: &[(f64, f64, f64, f64)]) -> (Vec<DailyBar>, Calendar) {
        let n = 40;
        let ds = dates(n);
        let mut bars: Vec<DailyBar> = ds
            .iter()
            .map(|d| bar(d, 100.0, 100.5, 99.5, 100.0, 1))
            .collect();
        for (i, (o, h, l, c)) in days.iter().enumerate() {
            bars[5 + i] = bar(&ds[5 + i], *o, *h, *l, *c, 1);
        }
        (bars, cal(n))
    }

    fn outcome(days: &[(f64, f64, f64, f64)]) -> Outcome {
        let (bars, cal) = path(days);
        simulate_trade(&cal.aligned(&bars), 5, 1.0, &TRIAL_8).unwrap()
    }

    fn net(entry: f64, exit: f64) -> f64 {
        (exit - entry - COST_PER_SIDE * (entry + exit)) / 2.0
    }

    #[test]
    fn trial_13_has_no_target_and_sells_at_the_21st_open() {
        let (bars, cal) = path(&[(100.0, 120.0, 99.5, 110.0)]);
        let o = simulate_trade(&cal.aligned(&bars), 5, 1.0, &TRIAL_13).unwrap();
        assert_eq!(
            (o.exit, o.r),
            (25, net(100.0, 100.0)),
            "a run far past 3 ATR isn't sold"
        );
        let stop = {
            let (bars, cal) = path(&[(100.0, 101.0, 99.0, 100.0), (100.0, 100.0, 97.0, 98.0)]);
            simulate_trade(&cal.aligned(&bars), 5, 1.0, &TRIAL_13).unwrap()
        };
        assert_eq!(
            (stop.exit, stop.r),
            (6, net(100.0, 98.0)),
            "the stop still works"
        );
        assert_eq!(
            (TRIAL_13.min_atr_fraction, TRIAL_13.stocks_only),
            (TRIAL_9.min_atr_fraction, TRIAL_9.stocks_only),
            "trial 9's filters"
        );
    }

    #[test]
    fn insider_days_are_signals_only_where_a_cluster_fired() {
        let n = 400;
        let (bars, cal) = (breakout_bars(n, n - 5), cal(n));
        let all = candidates(0, &bars, &cal, &TRIAL_13, &BTreeMap::new());
        assert!(!all.is_empty() && all.iter().all(|(_, c)| c.ratio.is_none()));
        let s = all[all.len() / 2].0;
        let events = BTreeMap::from([(s, 250_000.0)]);
        let fired: Vec<(usize, Option<f64>)> = candidates(0, &bars, &cal, &TRIAL_13, &events)
            .into_iter()
            .filter(|(_, c)| c.ratio.is_some())
            .map(|(d, c)| (d, c.ratio))
            .collect();
        assert_eq!(fired, [(s, Some(250_000.0))]);
    }

    #[test]
    fn exits_follow_the_fill_model() {
        let stop = outcome(&[(100.0, 101.0, 99.0, 100.0), (100.0, 100.0, 97.0, 98.0)]);
        assert_eq!((stop.exit, stop.r), (6, net(100.0, 98.0)));
        let both = outcome(&[(100.0, 104.0, 97.0, 100.0)]);
        assert_eq!(
            both.r,
            net(100.0, 98.0),
            "stop first on a day touching both"
        );
        let gap = outcome(&[(100.0, 101.0, 99.0, 100.0), (95.0, 96.0, 94.0, 95.0)]);
        assert_eq!(
            gap.r,
            net(100.0, 95.0),
            "a gap through the stop fills at the open"
        );
        let target = outcome(&[(100.0, 103.5, 99.5, 103.0)]);
        assert_eq!(target.r, net(100.0, 103.0));
        let touch = outcome(&[(100.0, 103.0, 99.5, 102.0)]);
        assert_eq!(
            touch.exit,
            5 + MAX_HOLD,
            "a touch of the target is not a fill"
        );
        let up = outcome(&[(100.0, 101.0, 99.5, 101.0), (105.0, 106.0, 104.0, 105.0)]);
        assert_eq!(
            up.r,
            net(100.0, 105.0),
            "a gap over the target fills at the open"
        );
        let time = outcome(&[(100.0, 100.5, 99.5, 100.0)]);
        assert_eq!(
            (time.exit, time.r),
            (15, net(100.0, 100.0)),
            "sold at the 11th open"
        );

        let (bars, cal) = path(&[(100.0, 100.5, 99.5, 101.0)]);
        let gone: Vec<DailyBar> = bars[..8].to_vec();
        let d = simulate_trade(&cal.aligned(&gone), 5, 1.0, &TRIAL_8).unwrap();
        assert_eq!(
            (d.exit, d.r),
            (7, net(100.0, 100.0)),
            "delisted: last close"
        );
    }

    /// A long uptrend with a volatile stretch 60–90 days before day `k`, so the breakout's ATR
    /// isn't the top of its last 100; then one breakout day `k` on high volume.
    fn breakout_bars(n: usize, k: usize) -> Vec<DailyBar> {
        let ds = dates(n);
        (0..n)
            .map(|i| {
                let p = 50.0 + i as f64 * 0.1 + if i % 2 == 0 { 0.3 } else { 0.0 };
                let swing = if (k - 90..k - 60).contains(&i) {
                    1.5
                } else {
                    0.4
                };
                if i == k {
                    bar(&ds[i], p, p + 2.0, p - 0.2, p + 1.9, 3_000_000)
                } else {
                    bar(&ds[i], p, p + swing, p - swing, p, 1_000_000)
                }
            })
            .collect()
    }

    #[test]
    fn desk_signal_fires_on_a_breakout_in_an_uptrend_only() {
        let (n, k) = (320, 300);
        let cal = cal(n);
        let c = candidates(0, &breakout_bars(n, k), &cal, &TRIAL_8, &BTreeMap::new());
        let at = |s: usize| c.iter().find(|(d, _)| *d == s).map(|(_, x)| x.ratio);
        assert!(at(k).flatten().is_some_and(|r| r >= 3.0), "{:?}", at(k));
        assert_eq!(
            at(k - 6).flatten(),
            None,
            "a quiet day is eligible but not a signal"
        );
        assert!(at(k - 6).is_some(), "eligible for the random baseline");

        let mut weak = breakout_bars(n, k);
        weak[k].volume = 1_200_000;
        let c = candidates(0, &weak, &cal, &TRIAL_8, &BTreeMap::new());
        assert_eq!(
            c.iter().find(|(d, _)| *d == k).unwrap().1.ratio,
            None,
            "volume < 1.5×"
        );

        let mut chase = breakout_bars(n, k);
        chase[k + 1].open = chase[k].close * 1.01;
        let c = candidates(0, &chase, &cal, &TRIAL_8, &BTreeMap::new());
        assert!(
            c.iter().all(|(d, _)| *d != k),
            "an entry > 0.5% above the close is skipped"
        );

        let thin: Vec<DailyBar> = breakout_bars(n, k)
            .into_iter()
            .map(|mut b| {
                b.volume /= 100;
                b
            })
            .collect();
        assert!(
            candidates(0, &thin, &cal, &TRIAL_8, &BTreeMap::new()).is_empty(),
            "under $20M a day"
        );
    }

    fn cand(symbol: usize, ratio: Option<f64>, entry: usize, exit: usize, r: f64) -> Cand {
        Cand {
            symbol,
            ratio,
            out: Outcome {
                entry,
                exit,
                r,
                stop_fraction: 0.02,
                at_data_end: false,
            },
        }
    }

    #[test]
    fn portfolio_ranks_caps_skips_held_and_stops_at_the_cut() {
        let names: Vec<String> = (0..8).map(|i| format!("S{i}")).collect();
        let mut days = vec![Vec::new(); 20];
        days[1] = (0..7)
            .map(|i| cand(i, Some(2.0 + i as f64), 2, 6, 1.0))
            .collect();
        days[2] = vec![
            cand(6, Some(9.0), 3, 5, 1.0),
            cand(7, Some(1.6), 3, 4, -1.0),
        ];
        days[7] = vec![cand(7, Some(1.6), 8, 30, 1.0)];
        let (t, per_day) = portfolio(&days, &names, 0, 20);
        let first: Vec<usize> = t
            .iter()
            .filter(|x| x.out.entry == 2)
            .map(|x| x.symbol)
            .collect();
        assert_eq!(first, vec![6, 5, 4, 3, 2], "best ratios, capped at 5");
        assert_eq!(per_day[2], 0, "full, and S6 already held");
        assert_eq!(per_day[7], 1, "slots free after the exits");
        assert!(
            t.iter().all(|x| x.out.exit < 20),
            "a trade open at the cut isn't returned"
        );
        let (t, _) = portfolio(&days, &names, 5, 20);
        assert!(t.iter().all(|x| x.out.entry >= 5));
    }

    #[test]
    fn baseline_draws_as_many_as_the_strategy_took() {
        let mut days = vec![Vec::new(); 3];
        days[0] = (0..10).map(|i| cand(i, None, 1, 2, i as f64)).collect();
        let b = baseline(&days, &[3, 0, 0], 10);
        assert_eq!(b.len(), 3 * RANDOM_SEEDS as usize);
        assert_eq!(b, baseline(&days, &[3, 0, 0], 10), "seeded");
    }

    #[test]
    fn study_gate_and_metrics() {
        let n = 120;
        let cal = cal(n);
        let spy: Vec<DailyBar> = dates(n)
            .iter()
            .enumerate()
            .map(|(i, d)| bar(d, 100.0, 100.0, 100.0, 100.0 + i as f64 * 0.01, 1))
            .collect();
        let trades: Vec<Taken> = (0..110)
            .map(|i| Taken {
                symbol: i,
                out: Outcome {
                    entry: 5 + i % 100,
                    exit: 6 + i % 100,
                    r: if i % 3 == 0 { -1.0 } else { 1.0 },
                    stop_fraction: 0.02,
                    at_data_end: false,
                },
            })
            .collect();
        let s = Study::new(trades, Vec::new(), 1..110, &cal, &spy).unwrap();
        assert!((s.profit_factor().unwrap() - 73.0 / 37.0).abs() < 1e-12);
        assert!(s.max_drawdown_r() >= 1.0);
        let g = s.gate(8);
        let by = |p: &str| {
            g.checks
                .iter()
                .find(|c| c.name.starts_with(p))
                .unwrap()
                .passed
        };
        assert!(!by("lower confidence bound beats"), "no baseline is a fail");
        assert!(by("enough trades"));
        assert!(!g.passed());
    }

    #[test]
    fn young_symbols_and_bad_prints_are_not_eligible() {
        let (n, k) = (320, 300);
        let cal = cal(n);
        let young: Vec<DailyBar> = breakout_bars(n, k)[100..].to_vec();
        let c = candidates(0, &young, &cal, &TRIAL_8, &BTreeMap::new());
        assert!(
            c.iter().all(|(s, _)| *s >= 100 + MIN_HISTORY - 1),
            "under 250 bars: not even in the baseline pool"
        );
        let mut bad = breakout_bars(n, k);
        bad[k + 2].high = f64::NAN;
        let on: Vec<Option<&DailyBar>> = cal
            .aligned(&bad)
            .into_iter()
            .map(|b| b.filter(|b| sane(b)))
            .collect();
        assert!(on[k + 2].is_none() && on[k + 1].is_some());
    }

    #[test]
    fn spy_months_end_with_the_research_sessions() {
        let n = 120;
        let cal = cal(n);
        let spy: Vec<DailyBar> = dates(n)
            .iter()
            .enumerate()
            .map(|(i, d)| bar(d, 100.0, 100.0, 100.0, 100.0 + i as f64, 1))
            .collect();
        // research ends after the first session of month 4 (index 60)
        let s = Study::new(Vec::new(), Vec::new(), 1..61, &cal, &spy).unwrap();
        assert_eq!(s.months.last().unwrap(), "2016-04");
        let last = *s.spy_monthly.last().unwrap();
        assert!(
            (last - (160.0 / 159.0 - 1.0)).abs() < 1e-12,
            "only the sessions before the cut"
        );
    }

    #[test]
    fn trial_9_floor_and_fund_names() {
        let (n, k) = (320, 300);
        let cal = cal(n);
        let bars = breakout_bars(n, k);
        assert!(!candidates(0, &bars, &cal, &TRIAL_8, &BTreeMap::new()).is_empty());
        // the fixture's ATR is about 1-2% of an ~80 price at the breakout, well under 15%
        let strict = Rules {
            min_atr_fraction: 0.15,
            ..TRIAL_9
        };
        assert!(
            candidates(0, &bars, &cal, &strict, &BTreeMap::new()).is_empty(),
            "under the floor"
        );
        for f in [
            "iShares 0-3 Month Treasury Bond ETF",
            "SPDR Bloomberg 1-3 Month T-Bill ETF",
            "ProShares UltraPro QQQ",
            "Direxion Daily Semiconductor Bull 3X Shares",
            "PIMCO Enhanced Short Maturity Active Exchange-Traded Fund",
            "iPath Bloomberg Commodity Index Total Return ETN",
            "Some Income Fund",
        ] {
            assert!(is_fund(f), "{f}");
        }
        for s in [
            "Apple Inc. Common Stock",
            "Northern Trust Corporation",
            "Fundamental Global Inc.",
            "Invesco QQQ Trust, Series 1",
        ] {
            assert!(!is_fund(s), "{s}");
        }
    }

    #[test]
    fn signal_today_matches_the_backtest_and_scoring_waits_for_bars() {
        let (n, k) = (320, 300);
        let full = breakout_bars(n, k);
        // the calendar ends on the breakout day: tonight's view of it
        let ds = dates(n);
        let tonight = Calendar::new(
            &ds[..=k]
                .iter()
                .map(|d| bar(d, 1.0, 1.0, 1.0, 1.0, 1))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let sig = signal_today("X", &full[..=k], &tonight, &TRIAL_8).unwrap();
        let back = candidates(0, &full, &cal(n), &TRIAL_8, &BTreeMap::new());
        let then = back.iter().find(|(d, _)| *d == k).unwrap().1;
        assert_eq!(Some(sig.ratio), then.ratio, "same signal as the backtest");
        assert_eq!(sig.date, ds[k]);
        assert_eq!(
            signal_today("X", &full[..k], &tonight, &TRIAL_8),
            None,
            "no bar today"
        );
        assert_eq!(score_forward(&full[..=k], &tonight, &sig), Scored::Pending);
        let later = Calendar::new(
            &ds[..=k + 3]
                .iter()
                .map(|d| bar(d, 1.0, 1.0, 1.0, 1.0, 1))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(
            score_forward(&full[..=k + 3], &later, &sig),
            Scored::Pending,
            "a quiet hold isn't done until its time stop"
        );
        match score_forward(&full, &cal(n), &sig) {
            Scored::Done { r, .. } => assert!((r - then.out.r).abs() < 1e-12, "same outcome"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_missing_newest_bar_keeps_the_trade_open_until_it_looks_delisted() {
        let (n, k) = (320, 300);
        let full = breakout_bars(n, k);
        let ds = dates(n);
        let at = |m: usize| {
            Calendar::new(
                &ds[..m]
                    .iter()
                    .map(|d| bar(d, 1.0, 1.0, 1.0, 1.0, 1))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let sig = signal_today("X", &full[..=k], &at(k + 1), &TRIAL_8).unwrap();
        // the symbol's bars stop after k+3 while the calendar runs on
        let stopped = &full[..=k + 3];
        assert_eq!(
            score_forward(stopped, &at(k + 6), &sig),
            Scored::Pending,
            "3 sessions quiet"
        );
        match score_forward(stopped, &at(k + 3 + DELISTED_AFTER + 1), &sig) {
            Scored::Done { exit_date, .. } => assert_eq!(exit_date, ds[k + 3], "last close"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            score_forward(&full[..=k], &at(k + 2), &sig),
            Scored::Pending,
            "the entry bar may just be late"
        );
        let split = Signal {
            close: sig.close / 2.0,
            atr: sig.atr / 2.0,
            ..sig.clone()
        };
        assert_eq!(
            score_forward(&full, &at(n), &split),
            score_forward(&full, &at(n), &sig),
            "a split since the signal rescales its ATR"
        );
    }
}
