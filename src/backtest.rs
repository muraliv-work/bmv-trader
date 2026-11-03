//! Replays historical days through the real engine with a simulated broker (DESIGN §13).
//! The fill model is fixed here, before any results are seen, and errs against the strategy:
//! - limit orders (the entry and the target) fill at their own price, and only when a bar trades
//!   strictly through it (a touch is a miss); an entry can't fill after its 1-minute TTL
//! - a bar that fills the entry can stop it out but can't reach the target (worst order assumed)
//! - stop and target in one bar: the stop; a stop through a gap fills at min(stop, open)
//! - market fills (stops, flattens) pay slippage of 1¢ + 0.02% of the fill price per share
//! - partial fills at a seeded, repeatable rate: half the shares, the rest cancelled by the TTL
//! - a flatten fills at that symbol's last close minus slippage (at the stop if there is none)
//! - every symbol's bar for a minute reaches the broker before the engine sees any of them close
//!
//! Claude isn't backtested: its training data may already contain how these days ended (§13).

use crate::config::Mode;
use crate::engine::{Effect, Event, Settings, State};
use crate::money::{Price, Qty, Usd};
use crate::risk::{Limits, OrderIntent};
use crate::strategy::Bar;
use std::collections::{BTreeMap, VecDeque};

/// Matches the engine's entry time-to-live.
const ENTRY_TTL_SECS: i64 = 60;

/// One regular session's bars for any number of symbols.
#[derive(Clone, Debug, PartialEq)]
pub struct Day {
    pub date: String,
    pub open: i64,
    pub close: i64,
    pub bars: Vec<(String, Bar)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FillModel {
    pub slippage_cents: i64,
    pub slippage_bps: i64,
    /// Percent of entries that fill only half.
    pub partial_pct: u64,
    pub seed: u64,
    pub buying_power: Usd,
}

impl Default for FillModel {
    fn default() -> Self {
        FillModel {
            slippage_cents: 1,
            slippage_bps: 2,
            partial_pct: 10,
            seed: 1,
            buying_power: Usd::from_cents(2_500_000),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Stop,
    Target,
    Flatten,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trade {
    pub id: String,
    pub date: String,
    pub symbol: String,
    pub qty: Qty,
    pub entry: Price,
    pub exit: Price,
    pub reason: ExitReason,
    /// Planned risk per share (entry − stop).
    pub r: Usd,
    pub pnl: Usd,
}

impl Trade {
    /// P&L in units of the planned risk on the shares actually held (0 if that risk isn't positive,
    /// which a trade from the engine can't have).
    pub fn r_multiple(&self) -> f64 {
        let risk = (self.r * self.qty).cents();
        if risk > 0 {
            self.pnl.cents() as f64 / risk as f64
        } else {
            0.0
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DayResult {
    pub date: String,
    pub trades: Vec<Trade>,
    /// Entries cancelled without any fill: missed breakouts flatter a backtest if ignored.
    pub missed: u32,
    pub halted: bool,
}

struct Working {
    order: OrderIntent,
    eligible_from: i64,
    partial: bool,
    filled: bool,
}

/// Becomes a position the moment any shares fill (a partial fill's remainder is cancelled by the TTL).
struct Position {
    symbol: String,
    qty: Qty,
    entry: Price,
    stop: Price,
    target: Price,
    /// The bar it filled in, which can stop it out but can't reach the target.
    filled_at: i64,
}

struct Sim {
    model: FillModel,
    rng: u64,
    working: BTreeMap<String, Working>,
    open: BTreeMap<String, Position>,
    last_close: BTreeMap<String, f64>,
    day: DayResult,
}

/// Runs every day through one engine (so halts and limits behave as live). Rules-only: vetting must be off.
pub fn run(days: &[Day], settings: Settings, model: FillModel) -> Result<Vec<DayResult>, String> {
    if settings.vetting {
        return Err("backtests run rules-only; Claude can't be backtested (DESIGN §13)".into());
    }
    let mut engine = State::new(Limits::for_mode(Mode::Paper), settings)?;
    let mut sim = Sim {
        rng: model.seed.max(1),
        model,
        working: BTreeMap::new(),
        open: BTreeMap::new(),
        last_close: BTreeMap::new(),
        day: DayResult::default(),
    };
    let mut results = Vec::with_capacity(days.len());
    for day in days {
        sim.day = DayResult {
            date: day.date.clone(),
            ..DayResult::default()
        };
        sim.last_close.clear();
        sim.drive(
            &mut engine,
            Event::SessionStart {
                date: day.date.clone(),
                open: day.open,
                close: day.close,
                buying_power: sim.model.buying_power,
            },
        );
        let mut bars = day.bars.clone();
        bars.sort_by(|(sa, a), (sb, b)| a.ts.cmp(&b.ts).then_with(|| sa.cmp(sb)));
        for minute in bars.chunk_by(|(_, a), (_, b)| a.ts == b.ts) {
            for (symbol, bar) in minute {
                sim.on_bar(&mut engine, symbol, bar);
            }
            for (symbol, bar) in minute {
                sim.last_close.insert(symbol.clone(), bar.close);
                let bar = *bar;
                sim.drive(
                    &mut engine,
                    Event::Bar {
                        symbol: symbol.clone(),
                        bar,
                    },
                );
            }
            sim.drive(&mut engine, Event::Tick(minute[0].1.ts + 60));
        }
        // the clock runs to the close even if the last bars are missing
        sim.drive(&mut engine, Event::Tick(day.close - 5 * 60));
        sim.drive(&mut engine, Event::Tick(day.close));
        if !sim.open.is_empty() || !sim.working.is_empty() {
            return Err(format!(
                "{}: {} positions and {} orders still open after the close",
                day.date,
                sim.open.len(),
                sim.working.len()
            ));
        }
        sim.day.halted = engine.halted();
        results.push(std::mem::take(&mut sim.day));
    }
    Ok(results)
}

impl Sim {
    fn slip(&self, p: f64) -> f64 {
        let cents = self.model.slippage_cents as f64
            + (p * 100.0 * self.model.slippage_bps as f64 / 10_000.0).ceil();
        cents / 100.0
    }

    /// A market sell at `px` after slippage; if that isn't a valid price, the worst one (1¢).
    fn market_fill(&self, px: f64) -> Price {
        Price::from_f64_floor(px - self.slip(px)).unwrap_or(Price::PENNY)
    }

    /// xorshift64: repeatable "randomness" from the seed.
    fn roll(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng % 100
    }

    /// The broker's view of a bar, before the engine sees it close.
    fn on_bar(&mut self, engine: &mut State, symbol: &str, bar: &Bar) {
        let mut events = Vec::new();
        for (id, w) in self.working.iter_mut() {
            let expired = bar.ts >= w.eligible_from + ENTRY_TTL_SECS;
            if w.order.symbol() != symbol || bar.ts < w.eligible_from || expired || w.filled {
                continue;
            }
            let limit = w.order.entry().cents() as f64 / 100.0;
            if bar.low < limit {
                let full = w.order.qty();
                let qty = if w.partial {
                    Qty::new((full.get() / 2).max(1)).unwrap_or(full)
                } else {
                    full
                };
                w.filled = true;
                let o = &w.order;
                self.open.insert(
                    id.clone(),
                    Position {
                        symbol: o.symbol().to_string(),
                        qty,
                        entry: o.entry(),
                        stop: o.stop(),
                        target: o.target(),
                        filled_at: bar.ts,
                    },
                );
                events.push(Event::EntryFilled {
                    id: id.clone(),
                    cum_qty: qty,
                    avg_price: o.entry(),
                });
            }
        }
        // a full fill leaves nothing working; a partial's remainder waits for the TTL cancel
        self.working.retain(|id, w| {
            !(w.filled && self.open.get(id).is_some_and(|p| p.qty == w.order.qty()))
        });
        for e in events {
            self.drive(engine, e);
        }
        // exits for positions held at the start of this bar or filled in it
        let held: Vec<String> = self.open.keys().cloned().collect();
        for id in held {
            let Some(p) = self.open.get(&id) else {
                continue;
            };
            if p.symbol != symbol {
                continue;
            }
            let stop = p.stop.cents() as f64 / 100.0;
            let target = p.target.cents() as f64 / 100.0;
            let exit = if bar.low <= stop {
                let fill = stop.min(bar.open);
                Some((self.market_fill(fill), ExitReason::Stop))
            } else if bar.high > target && p.filled_at != bar.ts {
                Some((p.target, ExitReason::Target))
            } else {
                None
            };
            if let Some((price, reason)) = exit {
                self.close(engine, &id, price, reason);
            }
        }
    }

    fn close(&mut self, engine: &mut State, id: &str, price: Price, reason: ExitReason) {
        let Some(p) = self.open.remove(id) else {
            return;
        };
        self.day.trades.push(Trade {
            id: id.to_string(),
            date: self.day.date.clone(),
            r: p.entry - p.stop,
            pnl: (price - p.entry) * p.qty,
            symbol: p.symbol,
            qty: p.qty,
            entry: p.entry,
            exit: price,
            reason,
        });
        self.drive(
            engine,
            Event::Exited {
                id: id.to_string(),
                price,
            },
        );
    }

    /// Feeds an event to the engine and plays out every effect, including the broker's replies.
    fn drive(&mut self, engine: &mut State, first: Event) {
        let mut queue = VecDeque::from([first]);
        while let Some(event) = queue.pop_front() {
            let is_bar_ts = match &event {
                Event::Bar { bar, .. } => Some(bar.ts),
                _ => None,
            };
            for effect in engine.step(event) {
                match effect {
                    Effect::Submit { id, order } => {
                        // rules-only backtests submit only on a bar; anything else can never fill
                        let partial = self.roll() < self.model.partial_pct;
                        let eligible_from = is_bar_ts.map_or(i64::MAX, |ts| ts + 60);
                        self.working.insert(
                            id,
                            Working {
                                order,
                                eligible_from,
                                partial,
                                filled: false,
                            },
                        );
                    }
                    Effect::CancelEntry { id } => {
                        if let Some(w) = self.working.remove(&id) {
                            if !w.filled {
                                self.day.missed += 1;
                            }
                            queue.push_back(Event::EntryClosed { id });
                        }
                    }
                    Effect::FlattenAll => {
                        // drained one at a time: closing can trigger a nested flatten
                        while let Some(id) = self.open.keys().next().cloned() {
                            let Some(p) = self.open.get(&id) else { break };
                            let stop = p.stop.cents() as f64 / 100.0;
                            let last = self.last_close.get(&p.symbol).copied().unwrap_or(stop);
                            let px = self.market_fill(last);
                            self.close(engine, &id, px, ExitReason::Flatten);
                        }
                    }
                    Effect::Vet { .. } | Effect::Journal { .. } => {}
                }
            }
        }
    }
}

/// Summary metrics over all trades (DESIGN §13). Bootstrap and baselines live in the stats module.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub trades: usize,
    pub missed: u32,
    pub win_rate: f64,
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub max_drawdown_r: f64,
    pub pnl: Usd,
}

pub fn summarize(days: &[DayResult]) -> Summary {
    let trades: Vec<&Trade> = days.iter().flat_map(|d| &d.trades).collect();
    let rs: Vec<f64> = trades.iter().map(|t| t.r_multiple()).collect();
    let n = rs.len();
    let wins: f64 = rs.iter().filter(|r| **r > 0.0).sum();
    let losses: f64 = -rs.iter().filter(|r| **r < 0.0).sum::<f64>();
    let (mut equity, mut peak, mut max_dd) = (0.0f64, 0.0f64, 0.0f64);
    for r in &rs {
        equity += r;
        peak = peak.max(equity);
        max_dd = max_dd.max(peak - equity);
    }
    Summary {
        trades: n,
        missed: days.iter().map(|d| d.missed).sum(),
        win_rate: if n == 0 {
            0.0
        } else {
            rs.iter().filter(|r| **r > 0.0).count() as f64 / n as f64
        },
        expectancy_r: if n == 0 {
            0.0
        } else {
            rs.iter().sum::<f64>() / n as f64
        },
        // no winners (or no trades) is 0, never "infinitely good"
        profit_factor: if wins == 0.0 {
            0.0
        } else if losses == 0.0 {
            f64::INFINITY
        } else {
            wins / losses
        },
        max_drawdown_r: max_dd,
        pnl: trades.iter().fold(Usd::ZERO, |a, t| a + t.pnl),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::OrbParams;

    const OPEN: i64 = 1_704_205_800; // 2024-01-02 09:30 ET
    const CLOSE: i64 = OPEN + 390 * 60;

    fn bar(minute: i64, open: f64, high: f64, low: f64, close: f64, volume: u64) -> Bar {
        Bar {
            ts: OPEN + minute * 60,
            open,
            high,
            low,
            close,
            volume,
        }
    }

    /// Range 100–101 for 15 minutes, breakout close 101.50 at minute 15 (entry 101.50, stop 100.00,
    /// target 104.50, 24 shares), then `after` from minute 16.
    fn day(after: Vec<Bar>) -> Day {
        let mut bars: Vec<(String, Bar)> = (0..15)
            .map(|m| ("SPY".to_string(), bar(m, 100.5, 101.0, 100.0, 100.5, 1000)))
            .collect();
        bars.push(("SPY".into(), bar(15, 101.0, 101.6, 100.9, 101.5, 3000)));
        bars.extend(after.into_iter().map(|b| ("SPY".to_string(), b)));
        Day {
            date: "2024-01-02".into(),
            open: OPEN,
            close: CLOSE,
            bars,
        }
    }

    fn rules() -> Settings {
        Settings {
            vetting: false,
            orb: OrbParams::default(),
        }
    }

    fn no_partials() -> FillModel {
        FillModel {
            partial_pct: 0,
            ..FillModel::default()
        }
    }

    fn one(d: Day) -> DayResult {
        run(&[d], rules(), no_partials()).unwrap().remove(0)
    }

    #[test]
    fn a_winner_exits_at_the_target_without_slippage() {
        let r = one(day(vec![
            bar(16, 101.5, 101.8, 101.4, 101.7, 900), // trades through 101.50 → filled
            bar(17, 101.7, 104.6, 101.6, 104.0, 900), // reaches 104.50
        ]));
        assert_eq!(r.trades.len(), 1);
        let t = &r.trades[0];
        assert_eq!(
            (t.reason, t.qty.get(), t.entry.cents(), t.exit.cents()),
            (ExitReason::Target, 24, 10_150, 10_450)
        );
        assert!((t.r_multiple() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_loser_pays_slippage_at_the_stop() {
        let r = one(day(vec![
            bar(16, 101.5, 101.8, 101.4, 101.6, 900),
            bar(17, 101.0, 101.2, 99.9, 100.1, 900), // through the 100.00 stop
        ]));
        let t = &r.trades[0];
        // slip at $100 = 1¢ + ceil(0.02% of 10000¢) = 3¢ → 99.97
        assert_eq!((t.reason, t.exit.cents()), (ExitReason::Stop, 9_997));
        assert!(t.r_multiple() < -1.0);
    }

    #[test]
    fn a_gap_through_the_stop_fills_at_the_open() {
        let r = one(day(vec![
            bar(16, 101.5, 101.8, 101.4, 101.6, 900),
            bar(17, 98.0, 98.5, 97.5, 98.2, 900),
        ]));
        assert_eq!(r.trades[0].exit.cents(), 9_800 - 3);
    }

    #[test]
    fn stop_and_target_in_one_bar_counts_as_the_stop() {
        let r = one(day(vec![
            bar(16, 101.5, 101.8, 101.4, 101.6, 900),
            bar(17, 101.6, 105.0, 99.0, 104.0, 900),
        ]));
        assert_eq!(r.trades[0].reason, ExitReason::Stop);
    }

    #[test]
    fn the_fill_bar_cannot_reach_the_target() {
        // fills and spikes to 105 in the same minute: target not taken; next bar falls to the stop
        let r = one(day(vec![
            bar(16, 101.5, 105.0, 101.4, 104.8, 900),
            bar(17, 101.0, 101.0, 99.5, 99.8, 900),
        ]));
        assert_eq!(r.trades[0].reason, ExitReason::Stop);
    }

    #[test]
    fn a_touch_is_a_miss() {
        let r = one(day(vec![
            bar(16, 101.6, 102.0, 101.5, 101.9, 900),
            bar(17, 101.9, 102.5, 101.8, 102.4, 900),
        ]));
        assert!(r.trades.is_empty());
        assert_eq!(r.missed, 1);
    }

    #[test]
    fn an_entry_cannot_fill_after_its_ttl() {
        // minute 16 missing (IEX gap); minute 17 trades through the limit but the order has expired
        let r = one(day(vec![bar(17, 101.5, 101.8, 101.0, 101.7, 900)]));
        assert!(r.trades.is_empty());
        assert_eq!(r.missed, 1);
    }

    #[test]
    fn touching_the_target_is_not_a_fill() {
        let mut after = vec![
            bar(16, 101.5, 101.8, 101.4, 101.7, 900),
            bar(17, 101.7, 104.5, 101.6, 104.0, 900), // touches 104.50 exactly
        ];
        after.extend((18..390).map(|m| bar(m, 103.0, 103.2, 102.9, 103.0, 500)));
        let r = one(day(after));
        assert_eq!(r.trades[0].reason, ExitReason::Flatten);
    }

    #[test]
    fn a_gap_stop_pays_slippage_on_the_gap_price() {
        // gap open at $50: slip = 1¢ + ceil(0.02% × 5000¢ = 1.0) = 2¢ (on the $100 stop it would be 3¢)
        let r = one(day(vec![
            bar(16, 101.5, 101.8, 101.4, 101.6, 900),
            bar(17, 50.0, 50.5, 49.5, 50.2, 900),
        ]));
        assert_eq!(r.trades[0].exit.cents(), 5_000 - 2);
    }

    #[test]
    fn profit_factor_is_never_infinite_without_winners() {
        assert_eq!(summarize(&[]).profit_factor, 0.0);
        let mut breakeven = one(day(vec![bar(16, 101.5, 101.8, 101.4, 101.7, 900)]));
        for t in &mut breakeven.trades {
            t.pnl = Usd::ZERO;
        }
        assert_eq!(summarize(&[breakeven]).profit_factor, 0.0);
    }

    #[test]
    fn an_open_position_is_flattened_before_the_close() {
        let mut after = vec![bar(16, 101.5, 101.8, 101.4, 101.6, 900)];
        after.extend((17..390).map(|m| bar(m, 102.0, 102.2, 101.9, 102.0, 500)));
        let r = one(day(after));
        let t = &r.trades[0];
        assert_eq!(t.reason, ExitReason::Flatten);
        assert_eq!(t.exit.cents(), 10_200 - 4); // 1¢ + ceil(0.02% × 10200¢ = 2.04) = 4¢
        assert!(!r.halted);
    }

    #[test]
    fn flatten_happens_even_if_the_last_bars_are_missing() {
        let r = one(day(vec![bar(16, 101.5, 101.8, 101.4, 101.6, 900)]));
        assert_eq!(r.trades[0].reason, ExitReason::Flatten);
        assert!(!r.halted, "flat by the close");
    }

    #[test]
    fn partial_fills_hold_half_the_shares() {
        let model = FillModel {
            partial_pct: 100,
            ..FillModel::default()
        };
        let d = day(vec![
            bar(16, 101.5, 101.8, 101.4, 101.7, 900),
            bar(17, 101.7, 104.6, 101.6, 104.0, 900),
        ]);
        let r = run(&[d], rules(), model).unwrap().remove(0);
        assert_eq!(r.trades[0].qty.get(), 12);
        assert_eq!(r.missed, 0);
    }

    #[test]
    fn claude_cannot_be_backtested() {
        let s = Settings {
            vetting: true,
            ..rules()
        };
        assert!(run(&[], s, FillModel::default()).is_err());
    }

    #[test]
    fn replays_are_deterministic() {
        let days = vec![day(vec![
            bar(16, 101.5, 101.8, 101.4, 101.7, 900),
            bar(17, 101.7, 104.6, 101.6, 104.0, 900),
        ])];
        assert_eq!(
            run(&days, rules(), FillModel::default()),
            run(&days, rules(), FillModel::default())
        );
    }

    #[test]
    fn summary_metrics() {
        let mk = |pnl: i64| Trade {
            id: String::new(),
            date: String::new(),
            symbol: "X".into(),
            qty: Qty::new(1).unwrap(),
            entry: Price::from_cents(1_000).unwrap(),
            exit: Price::from_cents(1_000).unwrap(),
            reason: ExitReason::Target,
            r: Usd::from_cents(100),
            pnl: Usd::from_cents(pnl),
        };
        let days = [DayResult {
            trades: vec![mk(200), mk(-100), mk(-100), mk(200)],
            missed: 3,
            ..DayResult::default()
        }];
        let s = summarize(&days);
        assert_eq!((s.trades, s.missed, s.pnl), (4, 3, Usd::from_cents(200)));
        assert!((s.win_rate - 0.5).abs() < 1e-9);
        assert!((s.expectancy_r - 0.5).abs() < 1e-9);
        assert!((s.profit_factor - 2.0).abs() < 1e-9);
        assert!((s.max_drawdown_r - 2.0).abs() < 1e-9);
        assert_eq!(summarize(&[]).trades, 0);
    }
}
