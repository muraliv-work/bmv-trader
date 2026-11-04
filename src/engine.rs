//! The engine core (DESIGN §4): `State::step(Event) -> Vec<Effect>`, pure and deterministic.
//! The backtest drives it with a replay feeder and a simulated broker; the live loop (P3) drives it
//! with Alpaca's websockets and a 1 s `Tick`. Same function, same decisions. It never does I/O: it
//! asks for things (submit, cancel, vet, flatten, journal) by returning Effects.
//!
//! Contract for whoever drives it: a broker rejection of a submit arrives as `EntryClosed`; fills are
//! cumulative; `Exited` closes the whole position; cancels and flattens may be repeated and must be
//! idempotent. Anything the engine can't reconcile halts it and asks for a flatten.
// shortcut: state lives in memory only. A restart mid-session starts clean; P3 must rebuild it from
// Alpaca (positions, orders, equity − last_equity) before trading, per DESIGN §4.

use crate::money::{Price, Qty, Usd};
use crate::risk::{self, Exposure, Limits, Rejection, RiskState};
use crate::strategy::{self, Bar, Candidate, OrbParams};
use crate::vetting::Verdict;
use std::collections::BTreeMap;

/// No new entries in the last 30 minutes; flatten from 5 minutes before the close (DESIGN §6).
const ENTRY_CUTOFF_SECS: i64 = 30 * 60;
const FLATTEN_SECS: i64 = 5 * 60;
/// Re-send the flatten this often until the engine sees no positions.
const FLATTEN_RETRY_SECS: i64 = 60;
/// An entry not filled within one bar is cancelled (DESIGN §8).
const ENTRY_TTL_SECS: i64 = 60;
/// Claude has 10 s (DESIGN §8); a verdict that hasn't arrived by this deadline counts as a reject.
const VET_TIMEOUT_SECS: i64 = 30;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Send candidates to Claude (P4). Off: every candidate counts as approved.
    pub vetting: bool,
    pub orb: OrbParams,
    pub entry: Entry,
    /// Only take longs while both the market (SPY) and the symbol trend up today.
    pub trend_filter: bool,
}

/// The market the trend filter reads; its bars must be in the feed, or nothing trades.
pub const MARKET: &str = "SPY";

/// The newest market bar the trend filter may read is at most this old (one missing minute
/// tolerated); an older one means the market feed stalled, and a stale trend is unknown.
const MARKET_STALE_SECS: i64 = 120;

/// Which rule turns bars into candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The opening-range breakout (DESIGN §7).
    Orb,
    /// The P1 control: same exits and sizing, no signal (research only; refused in live mode).
    Random { seed: u64 },
    /// Gap-and-go (DESIGN §7b): needs `Event::PrevClose`, which only the backtest sends yet.
    Gap,
    /// VWAP pullback (DESIGN §7c).
    VwapPullback,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A new trading day; times are Unix seconds from Alpaca's calendar.
    SessionStart {
        date: String,
        open: i64,
        close: i64,
        buying_power: Usd,
    },
    /// The clock, so time-based rules fire even when no bars arrive.
    Tick(i64),
    /// Yesterday's last regular-hours close for a symbol, after SessionStart (gap-and-go only).
    PrevClose {
        symbol: String,
        close: f64,
    },
    Bar {
        symbol: String,
        bar: Bar,
    },
    /// Claude's answer for a candidate awaiting vetting.
    Vetted {
        id: String,
        verdict: Verdict,
    },
    /// **Cumulative** quantity filled so far on an entry, at the average price of all fills.
    EntryFilled {
        id: String,
        cum_qty: Qty,
        avg_price: Price,
    },
    /// The entry order is gone: cancelled, expired, or rejected. Anything filled stays open.
    EntryClosed {
        id: String,
    },
    /// The whole position closed (stop, target or flatten) at `price`.
    Exited {
        id: String,
        price: Price,
    },
    KillSwitch(bool),
    /// Something about the broker or the data can't be confirmed. New entries are blocked while
    /// any key is unresolved. Working orders are not cancelled and nothing latches.
    Unresolved {
        key: String,
        why: String,
    },
    /// Clears `key`. Resolving a key that isn't set does nothing and journals nothing.
    Resolved {
        key: String,
    },
    /// A position or order the engine can't explain: halt, cancel working, flatten.
    Unexplained {
        why: String,
    },
}

/// A trade that holds shares or could still get some, as the executor polls it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTrade {
    pub id: String,
    pub symbol: String,
    /// The entry order may still be working (Phase::Working).
    pub working: bool,
    /// Working: the ordered qty. Open: the held qty. The most shares Alpaca may show for this trade.
    pub qty: u32,
    /// Shares the engine believes it holds: 0, a partial fill, or all of them.
    pub filled: u32,
    /// Last bar close for the symbol, else the fill avg, else candidate.entry().
    pub mark: Price,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalKind {
    SessionStart,
    SessionRejected,
    BarIgnored,
    BarRejected,
    Candidate,
    Rejection,
    Verdict,
    VerdictIgnored,
    VetTimeout,
    OrderSubmit,
    Fill,
    FillIgnored,
    EntryClosed,
    Exit,
    /// The broker and the engine disagree; the engine halts and flattens.
    Unexpected,
    Halt,
    Flatten,
    KillSwitch,
    Unresolved,
    Resolved,
    Protect,
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    Vet {
        id: String,
        candidate: Candidate,
    },
    Submit {
        id: String,
        order: risk::OrderIntent,
    },
    CancelEntry {
        id: String,
    },
    FlattenAll,
    /// A partly filled entry was closed. Its filled shares have no stop (Alpaca only activates
    /// bracket legs on a complete fill), so they need an OCO, or a close if the OCO is refused.
    Protect {
        id: String,
    },
    Journal {
        kind: JournalKind,
        id: String,
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// Reserved at the pre-check size, an upper bound since Claude can only shrink it.
    Vetting {
        reserved: Qty,
        since: i64,
    },
    Working {
        ordered: Qty,
        filled: Option<(Qty, Price)>,
        submitted_at: i64,
        cancel_sent: bool,
    },
    Open {
        qty: Qty,
        entry: Price,
    },
    Closed,
    /// Never became an order; kept so the symbol isn't tried twice in a day.
    Rejected,
}

impl Phase {
    /// Holds shares, or could still get some.
    fn is_live(&self) -> bool {
        matches!(self, Phase::Working { .. } | Phase::Open { .. })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Trade {
    candidate: Candidate,
    phase: Phase,
}

#[derive(Clone, Debug, PartialEq)]
struct Session {
    date: String,
    open: i64,
    close: i64,
    cutoff_done: bool,
    last_flatten: Option<i64>,
    not_flat_alarmed: bool,
}

#[derive(Debug)]
pub struct State {
    allowed: risk::Allowed,
    limits: Limits,
    settings: Settings,
    session: Option<Session>,
    now: i64,
    bars: BTreeMap<String, Vec<Bar>>,
    marks: BTreeMap<String, Price>,
    /// Today's `Event::PrevClose` values; cleared at SessionStart.
    prev_close: BTreeMap<String, f64>,
    trades: BTreeMap<String, Trade>,
    seq: u32,
    kill_switch: bool,
    halted: bool,
    realized: Usd,
    consecutive_losses: u32,
    buying_power: Usd,
    /// Unconfirmed broker or data state, key → why. Non-empty blocks new entries; survives SessionStart.
    unresolved: BTreeMap<String, String>,
}

impl State {
    /// Refuses to build an engine whose risk gate or strategy settings fail their self-checks.
    /// `allowed` is the ticker allow-list the gate applies; `Allowed::Any` for research only.
    pub fn new(limits: Limits, settings: Settings, allowed: risk::Allowed) -> Result<Self, String> {
        risk::self_test(&limits)?;
        settings.orb.validate()?;
        if limits.mode() == crate::config::Mode::Live && settings.entry != Entry::Orb {
            return Err(format!(
                "only ORB may trade live; {:?} is research only",
                settings.entry
            ));
        }
        Ok(State {
            limits,
            settings,
            session: None,
            now: 0,
            bars: BTreeMap::new(),
            marks: BTreeMap::new(),
            prev_close: BTreeMap::new(),
            trades: BTreeMap::new(),
            seq: 0,
            kill_switch: false,
            halted: false,
            realized: Usd::ZERO,
            consecutive_losses: 0,
            buying_power: Usd::ZERO,
            unresolved: BTreeMap::new(),
            allowed,
        })
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    /// Trades in Phase::Working or Phase::Open, in id order. The executor polls exactly these.
    pub fn live_trades(&self) -> Vec<LiveTrade> {
        self.trades
            .iter()
            .filter_map(|(id, t)| {
                let (working, qty, filled, avg) = match t.phase {
                    Phase::Working {
                        ordered, filled, ..
                    } => (
                        true,
                        ordered.get(),
                        filled.map_or(0, |(q, _)| q.get()),
                        filled.map(|(_, p)| p),
                    ),
                    Phase::Open { qty, entry } => (false, qty.get(), qty.get(), Some(entry)),
                    _ => return None,
                };
                let symbol = t.candidate.symbol();
                let mark = self
                    .marks
                    .get(symbol)
                    .copied()
                    .or(avg)
                    .unwrap_or(t.candidate.entry());
                Some(LiveTrade {
                    id: id.clone(),
                    symbol: symbol.to_string(),
                    working,
                    qty,
                    filled,
                    mark,
                })
            })
            .collect()
    }

    /// Realized plus unrealized: open and partly filled positions marked at their last bar close.
    pub fn pnl_today(&self) -> Usd {
        self.trades.values().fold(self.realized, |acc, t| {
            let (qty, entry) = match t.phase {
                Phase::Open { qty, entry } => (qty, entry),
                Phase::Working {
                    filled: Some((qty, entry)),
                    ..
                } => (qty, entry),
                _ => return acc,
            };
            let mark = self
                .marks
                .get(t.candidate.symbol())
                .copied()
                .unwrap_or(entry);
            acc + (mark - entry) * qty
        })
    }

    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        match event {
            Event::SessionStart {
                date,
                open,
                close,
                buying_power,
            } => self.start_session(date, open, close, buying_power, &mut out),
            Event::Tick(ts) => {
                self.now = self.now.max(ts);
                self.housekeeping(&mut out);
            }
            Event::PrevClose { symbol, close } => {
                if self.session.is_some() {
                    self.prev_close.insert(symbol, close);
                }
            }
            Event::Bar { symbol, bar } => self.on_bar(symbol, bar, &mut out),
            Event::Vetted { id, verdict } => self.on_vetted(id, verdict, &mut out),
            Event::EntryFilled {
                id,
                cum_qty,
                avg_price,
            } => self.on_filled(id, cum_qty, avg_price, &mut out),
            Event::EntryClosed { id } => self.on_entry_closed(id, &mut out),
            Event::Exited { id, price } => self.on_exited(id, price, &mut out),
            Event::KillSwitch(on) => {
                self.kill_switch = on;
                journal(
                    &mut out,
                    JournalKind::KillSwitch,
                    "",
                    if on { "on" } else { "off" },
                );
                if on {
                    self.cancel_working(&mut out, |_, _| true);
                }
            }
            Event::Unresolved { key, why } => {
                if let std::collections::btree_map::Entry::Vacant(v) = self.unresolved.entry(key) {
                    journal(&mut out, JournalKind::Unresolved, v.key(), &why);
                    v.insert(why);
                }
            }
            Event::Resolved { key } => {
                if self.unresolved.remove(&key).is_some() {
                    journal(&mut out, JournalKind::Resolved, &key, "");
                }
            }
            Event::Unexplained { why } => self.unexpected(&mut out, "", &why),
        }
        out
    }

    fn start_session(
        &mut self,
        date: String,
        open: i64,
        close: i64,
        bp: Usd,
        out: &mut Vec<Effect>,
    ) {
        if self.session.as_ref().is_some_and(|s| s.date == date) {
            // A repeat must not wipe today's halts and P&L.
            return journal(
                out,
                JournalKind::SessionRejected,
                "",
                &format!("{date}: already started"),
            );
        }
        // Unix seconds between 2001 and 2286, and long enough for the opening range and the cutoff.
        let plausible = (1_000_000_000..10_000_000_000).contains(&open)
            && close - open > ENTRY_CUTOFF_SECS + 30 * 60;
        if !plausible {
            self.session = None;
            self.halted = true;
            return journal(
                out,
                JournalKind::SessionRejected,
                "",
                &format!("{date}: open {open} close {close}"),
            );
        }
        self.trades.retain(|_, t| t.phase.is_live());
        if self.trades.is_empty() {
            self.halted = false;
        } else {
            // Should be impossible (flat by close): refuse to trade and try to get flat.
            self.halted = true;
            let n = self.trades.len();
            journal(
                out,
                JournalKind::Halt,
                "",
                &format!("{n} positions/orders left from the previous session"),
            );
            self.cancel_working(out, |_, _| true);
            out.push(Effect::FlattenAll);
        }
        self.session = Some(Session {
            date: date.clone(),
            open,
            close,
            cutoff_done: false,
            last_flatten: None,
            not_flat_alarmed: false,
        });
        self.now = open;
        self.bars.clear();
        self.marks.clear();
        self.prev_close.clear();
        self.seq = 0;
        self.realized = Usd::ZERO;
        self.consecutive_losses = 0;
        self.buying_power = bp;
        journal(out, JournalKind::SessionStart, "", &date);
    }

    fn on_bar(&mut self, symbol: String, bar: Bar, out: &mut Vec<Effect>) {
        let Some(s) = &self.session else {
            return journal(
                out,
                JournalKind::BarIgnored,
                "",
                &format!("{symbol}: no session"),
            );
        };
        let (open, close) = (s.open, s.close);
        if bar.ts < open || bar.ts >= close {
            return journal(
                out,
                JournalKind::BarIgnored,
                "",
                &format!("{symbol}: outside session"),
            );
        }
        let mark = Price::from_f64_floor(bar.close);
        if !bar.is_sane() || mark.is_none() {
            journal(
                out,
                JournalKind::BarRejected,
                "",
                &format!("{symbol}: {bar:?}"),
            );
            if self
                .trades
                .values()
                .any(|t| t.candidate.symbol() == symbol && t.phase.is_live())
            {
                self.halt(out, &format!("bad data for {symbol}, which has a position"));
            }
            return;
        }
        let series = self.bars.entry(symbol.clone()).or_default();
        if series.last().is_some_and(|last| bar.ts <= last.ts) {
            return journal(
                out,
                JournalKind::BarIgnored,
                "",
                &format!("{symbol}: out of order at {}", bar.ts),
            );
        }
        series.push(bar);
        if let Some(m) = mark {
            self.marks.insert(symbol.clone(), m);
        }
        self.now = self.now.max(bar.ts + 60);
        self.housekeeping(out);

        let (bars, p) = (&self.bars[&symbol], &self.settings.orb);
        if self.settings.trend_filter {
            let up = |b: &[Bar]| strategy::trend(b, p.min_or_bars) == strategy::Trend::Up;
            // The market's minutes strictly before this bar: its same-minute bar may or may not
            // have arrived yet (replay sorts by symbol), so using it would make results order-dependent.
            let market = self
                .bars
                .get(MARKET)
                .map_or(&[][..], |m| &m[..m.partition_point(|b| b.ts < bar.ts)]);
            let fresh = market
                .last()
                .is_some_and(|m| bar.ts - m.ts <= MARKET_STALE_SECS);
            if !(fresh && up(market) && up(bars)) {
                return;
            }
        }
        let candidate = match self.settings.entry {
            Entry::Orb => strategy::orb_long(&symbol, open, bars, p),
            Entry::Random { seed } => strategy::random_long(&symbol, open, bars, p, seed),
            Entry::Gap => self
                .prev_close
                .get(&symbol)
                .and_then(|&pc| strategy::gap_long(&symbol, open, bars, pc, p)),
            Entry::VwapPullback => strategy::vwap_pullback_long(&symbol, open, bars, p),
        };
        if let Some(c) = candidate {
            self.on_candidate(c, out);
        }
    }

    /// Everything that depends only on the clock.
    fn housekeeping(&mut self, out: &mut Vec<Effect>) {
        let now = self.now;
        self.cancel_working(out, |t, now| matches!(t.phase, Phase::Working { submitted_at, .. } if now >= submitted_at + ENTRY_TTL_SECS));
        let timed_out: Vec<String> = self
            .trades
            .iter()
            .filter(|(_, t)| matches!(t.phase, Phase::Vetting { since, .. } if now >= since + VET_TIMEOUT_SECS))
            .map(|(id, _)| id.clone())
            .collect();
        for id in timed_out {
            self.drop_vetting(&id, "no verdict in time", out);
        }
        self.end_of_day(out);
        self.latch_halts(out);
    }

    fn drop_vetting(&mut self, id: &str, why: &str, out: &mut Vec<Effect>) {
        if let Some(t) = self.trades.get_mut(id) {
            t.phase = Phase::Rejected;
            journal(out, JournalKind::VetTimeout, id, why);
        }
    }

    /// Sends one cancel per working entry that matches.
    fn cancel_working(&mut self, out: &mut Vec<Effect>, matches: impl Fn(&Trade, i64) -> bool) {
        let now = self.now;
        for (id, t) in self.trades.iter_mut() {
            let hit = matches(t, now);
            if let Phase::Working { cancel_sent, .. } = &mut t.phase
                && hit
                && !*cancel_sent
            {
                *cancel_sent = true;
                out.push(Effect::CancelEntry { id: id.clone() });
            }
        }
    }

    fn end_of_day(&mut self, out: &mut Vec<Effect>) {
        let live = self.trades.values().any(|t| t.phase.is_live());
        let now = self.now;
        let Some(s) = self.session.as_mut() else {
            return;
        };
        let close = s.close;
        let cutoff = now > close - ENTRY_CUTOFF_SECS && !s.cutoff_done;
        s.cutoff_done |= cutoff;
        let flatten = now >= close - FLATTEN_SECS
            && match s.last_flatten {
                None => true,
                Some(t) => live && now >= t + FLATTEN_RETRY_SECS,
            };
        if flatten {
            s.last_flatten = Some(now);
        }
        let not_flat = now >= close && live && !s.not_flat_alarmed;
        s.not_flat_alarmed |= not_flat;

        if cutoff {
            let pending: Vec<String> = self
                .trades
                .iter()
                .filter(|(_, t)| matches!(t.phase, Phase::Vetting { .. }))
                .map(|(id, _)| id.clone())
                .collect();
            for id in pending {
                self.drop_vetting(&id, "entry cutoff", out);
            }
        }
        if cutoff || flatten {
            self.cancel_working(out, |_, _| true);
        }
        if flatten {
            out.push(Effect::FlattenAll);
            journal(
                out,
                JournalKind::Flatten,
                "",
                if live {
                    "positions open"
                } else {
                    "close − 5 min"
                },
            );
        }
        if not_flat {
            self.halt(out, "still holding positions after the close");
        }
    }

    /// Halts last the rest of the day, even if P&L recovers (DESIGN §9).
    fn latch_halts(&mut self, out: &mut Vec<Effect>) {
        if self.halted {
            return;
        }
        let pnl = self.pnl_today();
        if pnl <= Usd::ZERO - self.limits.daily_loss_halt() {
            self.halt(out, &format!("daily loss {pnl}"));
        } else if self.consecutive_losses >= self.limits.consecutive_loss_halt() {
            self.halt(
                out,
                &format!("{} consecutive losses", self.consecutive_losses),
            );
        }
    }

    fn halt(&mut self, out: &mut Vec<Effect>, why: &str) {
        if !self.halted {
            self.halted = true;
            journal(out, JournalKind::Halt, "", why);
        }
        self.cancel_working(out, |_, _| true);
    }

    /// The broker and the engine disagree: stop trading and get flat.
    fn unexpected(&mut self, out: &mut Vec<Effect>, id: &str, why: &str) {
        journal(out, JournalKind::Unexpected, id, why);
        self.halt(out, why);
        out.push(Effect::FlattenAll);
    }

    fn on_candidate(&mut self, c: Candidate, out: &mut Vec<Effect>) {
        let Some(s) = &self.session else {
            return journal(
                out,
                JournalKind::Candidate,
                "",
                "candidate outside a session ignored",
            );
        };
        self.seq += 1;
        let id = format!("bmv-{}-{}-{}", s.date, c.symbol(), self.seq);
        journal(out, JournalKind::Candidate, &id, c.reason());
        if self
            .trades
            .values()
            .any(|t| t.candidate.symbol() == c.symbol())
        {
            return journal(
                out,
                JournalKind::Rejection,
                &id,
                "duplicate candidate for symbol today",
            );
        }
        // Pre-check as if approved, so Claude is only asked about trades that could happen.
        let phase = match risk::gate(&c, Verdict::Approve, &self.risk_state(None), &self.limits) {
            Ok(o) => Phase::Vetting {
                reserved: o.qty(),
                since: self.now,
            },
            Err(why) => {
                reject(out, &id, why);
                Phase::Rejected
            }
        };
        let vet = matches!(phase, Phase::Vetting { .. });
        let candidate = c.clone();
        self.trades.insert(
            id.clone(),
            Trade {
                candidate: c,
                phase,
            },
        );
        match (vet, self.settings.vetting) {
            (false, _) => {} // rejected at the pre-check, already journaled
            (true, true) => out.push(Effect::Vet { id, candidate }),
            (true, false) => self.submit(id, Verdict::Approve, out),
        }
    }

    fn on_vetted(&mut self, id: String, verdict: Verdict, out: &mut Vec<Effect>) {
        match self.trades.get(&id) {
            Some(t) if matches!(t.phase, Phase::Vetting { .. }) => {
                journal(out, JournalKind::Verdict, &id, &format!("{verdict:?}"));
                self.submit(id, verdict, out);
            }
            _ => journal(
                out,
                JournalKind::VerdictIgnored,
                &id,
                "no candidate awaiting vetting",
            ),
        }
    }

    /// Runs the gate again on the current state (it may have changed while Claude thought).
    fn submit(&mut self, id: String, verdict: Verdict, out: &mut Vec<Effect>) {
        let snapshot = self.risk_state(Some(&id));
        let now = self.now;
        let Some(t) = self.trades.get_mut(&id) else {
            return journal(
                out,
                JournalKind::Unexpected,
                &id,
                "submit for a missing trade",
            );
        };
        match risk::gate(&t.candidate, verdict, &snapshot, &self.limits) {
            Ok(order) => {
                t.phase = Phase::Working {
                    ordered: order.qty(),
                    filled: None,
                    submitted_at: now,
                    cancel_sent: false,
                };
                let detail = format!(
                    "{} × {} stop {} target {}",
                    order.qty().get(),
                    order.entry(),
                    order.stop(),
                    order.target()
                );
                journal(out, JournalKind::OrderSubmit, &id, &detail);
                out.push(Effect::Submit { id, order });
            }
            Err(why) => {
                t.phase = Phase::Rejected;
                reject(out, &id, why);
            }
        }
    }

    fn on_filled(&mut self, id: String, cum_qty: Qty, avg_price: Price, out: &mut Vec<Effect>) {
        let Some(t) = self.trades.get_mut(&id) else {
            return self.unexpected(out, &id, "fill for an order this engine didn't place");
        };
        let detail = format!("{} @ {avg_price}", cum_qty.get());
        match t.phase {
            Phase::Working {
                filled: Some((q, _)),
                ..
            } if cum_qty <= q => {
                journal(
                    out,
                    JournalKind::FillIgnored,
                    &id,
                    &format!("stale: {detail}, already {}", q.get()),
                );
            }
            Phase::Working {
                ordered,
                submitted_at,
                cancel_sent,
                ..
            } => {
                t.phase = if cum_qty >= ordered {
                    Phase::Open {
                        qty: cum_qty,
                        entry: avg_price,
                    }
                } else {
                    Phase::Working {
                        ordered,
                        filled: Some((cum_qty, avg_price)),
                        submitted_at,
                        cancel_sent,
                    }
                };
                journal(out, JournalKind::Fill, &id, &detail);
                if cum_qty > ordered {
                    self.unexpected(
                        out,
                        &id,
                        &format!("overfill: {detail} for {} ordered", ordered.get()),
                    );
                }
            }
            Phase::Open { qty, .. } if cum_qty <= qty => {
                journal(
                    out,
                    JournalKind::FillIgnored,
                    &id,
                    &format!("stale: {detail}"),
                );
            }
            Phase::Open { .. } => {
                t.phase = Phase::Open {
                    qty: cum_qty,
                    entry: avg_price,
                };
                self.unexpected(out, &id, &format!("fill after the entry closed: {detail}"));
            }
            Phase::Vetting { .. } | Phase::Closed | Phase::Rejected => {
                let phase = format!("{:?}", t.phase);
                self.unexpected(out, &id, &format!("fill while {phase}: {detail}"));
            }
        }
    }

    fn on_entry_closed(&mut self, id: String, out: &mut Vec<Effect>) {
        let Some(t) = self.trades.get_mut(&id) else {
            return journal(out, JournalKind::EntryClosed, &id, "unknown order, ignored");
        };
        match t.phase {
            Phase::Working { filled, .. } => {
                t.phase = match filled {
                    Some((qty, entry)) => Phase::Open { qty, entry },
                    None => Phase::Closed,
                };
                journal(
                    out,
                    JournalKind::EntryClosed,
                    &id,
                    &format!("now {:?}", t.phase),
                );
                if filled.is_some() {
                    journal(out, JournalKind::Protect, &id, "partial fill has no stop");
                    out.push(Effect::Protect { id });
                }
            }
            ref other => {
                let other = format!("{other:?}");
                journal(
                    out,
                    JournalKind::EntryClosed,
                    &id,
                    &format!("ignored: trade is {other}"),
                );
            }
        }
    }

    fn on_exited(&mut self, id: String, price: Price, out: &mut Vec<Effect>) {
        let Some(t) = self.trades.get_mut(&id) else {
            return self.unexpected(out, &id, "exit for a position this engine doesn't hold");
        };
        let (qty, entry, cancel_rest) = match t.phase {
            Phase::Open { qty, entry } => (qty, entry, false),
            // a bracket leg filled on a partly filled entry
            Phase::Working {
                filled: Some((qty, entry)),
                cancel_sent,
                ..
            } => (qty, entry, !cancel_sent),
            ref other => {
                let other = format!("{other:?}");
                return self.unexpected(out, &id, &format!("exit while {other}"));
            }
        };
        t.phase = Phase::Closed;
        if cancel_rest {
            out.push(Effect::CancelEntry { id: id.clone() });
        }
        let pnl = (price - entry) * qty;
        self.realized = self.realized + pnl;
        self.consecutive_losses = if pnl < Usd::ZERO {
            self.consecutive_losses + 1
        } else {
            0
        };
        journal(
            out,
            JournalKind::Exit,
            &id,
            &format!("{} @ {price}: {pnl}", qty.get()),
        );
        self.latch_halts(out);
    }

    /// The gate's view of now. `exclude` leaves out a trade's own reservation when re-gating it.
    fn risk_state(&self, exclude: Option<&str>) -> RiskState {
        let entries_allowed = self
            .session
            .as_ref()
            .is_some_and(|s| self.now >= s.open && self.now <= s.close - ENTRY_CUTOFF_SECS);
        let others = || {
            self.trades
                .iter()
                .filter(|(id, _)| Some(id.as_str()) != exclude)
        };
        let mut committed = Usd::ZERO;
        let open = others()
            .filter_map(|(_, t)| {
                let risk = match t.phase {
                    Phase::Vetting { reserved: q, .. } | Phase::Working { ordered: q, .. } => {
                        committed = committed + t.candidate.entry() * q;
                        t.candidate.r() * q
                    }
                    Phase::Open { qty, entry } => {
                        committed = committed + entry * qty;
                        (entry - t.candidate.stop()) * qty
                    }
                    Phase::Closed | Phase::Rejected => return None,
                };
                Some(Exposure::new(t.candidate.symbol(), risk))
            })
            .collect();
        RiskState {
            kill_switch: self.kill_switch,
            // unknown means stop: unconfirmed broker or data state blocks entries without latching
            halted: self.halted || !self.unresolved.is_empty(),
            entries_allowed,
            pnl_today: self.pnl_today(),
            buying_power: self.buying_power - committed,
            consecutive_losses: self.consecutive_losses,
            traded_today: others()
                .filter(|(_, t)| t.phase != Phase::Rejected)
                .map(|(_, t)| t.candidate.symbol().to_string())
                .collect(),
            open,
            allowed: self.allowed.clone(),
        }
    }
}

fn journal(out: &mut Vec<Effect>, kind: JournalKind, id: &str, detail: &str) {
    out.push(Effect::Journal {
        kind,
        id: id.to_string(),
        detail: detail.to_string(),
    });
}

fn reject(out: &mut Vec<Effect>, id: &str, why: Rejection) {
    journal(out, JournalKind::Rejection, id, &why.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use crate::vetting::Factor;

    const OPEN: i64 = 1_704_205_800; // 2024-01-02 09:30 ET
    const CLOSE: i64 = OPEN + 390 * 60;
    const SPY1: &str = "bmv-2024-01-02-SPY-1";

    fn engine(vetting: bool) -> State {
        let mut e = State::new(
            Limits::for_mode(Mode::Paper),
            Settings {
                vetting,
                orb: OrbParams::default(),
                entry: Entry::Orb,
                trend_filter: false,
            },
            risk::Allowed::Any,
        )
        .unwrap();
        e.step(start("2024-01-02", 0));
        e
    }

    fn start(date: &str, day: i64) -> Event {
        Event::SessionStart {
            date: date.into(),
            open: OPEN + day * 86_400,
            close: CLOSE + day * 86_400,
            buying_power: Usd::from_cents(10_000_000),
        }
    }

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

    fn feed(e: &mut State, symbol: &str, bars: impl IntoIterator<Item = Bar>) -> Vec<Effect> {
        bars.into_iter()
            .flat_map(|bar| {
                e.step(Event::Bar {
                    symbol: symbol.into(),
                    bar,
                })
            })
            .collect()
    }

    fn range_bars() -> Vec<Bar> {
        (0..15).map(|m| bar(m, 101.0, 100.0, 100.5, 1000)).collect()
    }

    /// A 15-minute range 100–101 then a breakout close at 101.50 (R $1.50 → 24 shares, notional-capped).
    fn breakout_day(e: &mut State, symbol: &str) -> Vec<Effect> {
        let mut bars = range_bars();
        bars.push(bar(15, 101.6, 100.9, 101.5, 3000));
        feed(e, symbol, bars)
    }

    fn submits(fx: &[Effect]) -> Vec<(&str, u32)> {
        fx.iter()
            .filter_map(|f| match f {
                Effect::Submit { id, order } => Some((id.as_str(), order.qty().get())),
                _ => None,
            })
            .collect()
    }

    fn of_kind(fx: &[Effect], want: JournalKind) -> Vec<&str> {
        fx.iter()
            .filter_map(|f| match f {
                Effect::Journal { kind, detail, .. } if *kind == want => Some(detail.as_str()),
                _ => None,
            })
            .collect()
    }

    fn rejections(fx: &[Effect]) -> Vec<&str> {
        of_kind(fx, JournalKind::Rejection)
    }

    fn cancels(fx: &[Effect]) -> usize {
        fx.iter()
            .filter(|f| matches!(f, Effect::CancelEntry { .. }))
            .count()
    }

    fn flattens(fx: &[Effect]) -> usize {
        fx.iter().filter(|f| **f == Effect::FlattenAll).count()
    }

    fn qty(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }
    fn price(c: i64) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn fill(id: &str, n: u32, cents: i64) -> Event {
        Event::EntryFilled {
            id: id.into(),
            cum_qty: qty(n),
            avg_price: price(cents),
        }
    }
    fn exit(id: &str, cents: i64) -> Event {
        Event::Exited {
            id: id.into(),
            price: price(cents),
        }
    }

    // --- start-up and sessions -----------------------------------------------------------------

    #[test]
    fn engine_refuses_to_start_with_bad_settings() {
        let bad = Settings {
            vetting: false,
            orb: OrbParams {
                atr_len: 0,
                ..OrbParams::default()
            },
            entry: Entry::Orb,
            trend_filter: false,
        };
        assert!(State::new(Limits::for_mode(Mode::Paper), bad, risk::Allowed::Any).is_err());
        let random = Settings {
            vetting: false,
            orb: OrbParams::default(),
            entry: Entry::Random { seed: 1 },
            trend_filter: false,
        };
        assert!(
            State::new(
                Limits::for_mode(Mode::Paper),
                random.clone(),
                risk::Allowed::Any
            )
            .is_ok()
        );
        assert!(
            State::new(Limits::for_mode(Mode::Live), random, risk::Allowed::Any).is_err(),
            "the baseline never runs live"
        );
    }

    #[test]
    fn implausible_session_times_halt_the_day() {
        let mut e = State::new(
            Limits::for_mode(Mode::Paper),
            Settings {
                vetting: false,
                orb: OrbParams::default(),
                entry: Entry::Orb,
                trend_filter: false,
            },
            risk::Allowed::Any,
        )
        .unwrap();
        let ms = Event::SessionStart {
            date: "x".into(),
            open: OPEN * 1000,
            close: CLOSE * 1000,
            buying_power: Usd::ZERO,
        };
        assert_eq!(of_kind(&e.step(ms), JournalKind::SessionRejected).len(), 1);
        assert!(e.halted());
        let backwards = Event::SessionStart {
            date: "y".into(),
            open: CLOSE,
            close: OPEN,
            buying_power: Usd::ZERO,
        };
        assert_eq!(
            of_kind(&e.step(backwards), JournalKind::SessionRejected).len(),
            1
        );
    }

    #[test]
    fn a_repeated_session_start_does_not_reset_the_day() {
        let mut e = Engine::paper_with_loss(-50_016);
        assert!(e.halted());
        let fx = e.step(start("2024-01-02", 0));
        assert_eq!(
            of_kind(&fx, JournalKind::SessionRejected),
            ["2024-01-02: already started"]
        );
        assert!(e.halted());
    }

    #[test]
    fn a_new_session_halts_and_flattens_if_positions_were_left_open() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 24, 10_150));
        let fx = e.step(start("2024-01-03", 1));
        assert!(e.halted());
        assert_eq!(flattens(&fx), 1);
        e.step(exit(SPY1, 10_150));
        e.step(start("2024-01-04", 2));
        assert!(!e.halted());
    }

    // --- the candidate pipeline ----------------------------------------------------------------

    #[test]
    fn rules_only_breakout_submits_one_sized_order() {
        let mut e = engine(false);
        let fx = breakout_day(&mut e, "SPY");
        assert_eq!(submits(&fx), [(SPY1, 24)]);
        assert_eq!(of_kind(&fx, JournalKind::Candidate).len(), 1);
    }

    #[test]
    fn vetting_waits_for_claude_and_can_only_shrink() {
        let mut e = engine(true);
        let fx = breakout_day(&mut e, "SPY");
        assert!(submits(&fx).is_empty());
        assert!(
            fx.iter()
                .any(|f| matches!(f, Effect::Vet { id, .. } if id == SPY1))
        );
        let half = Verdict::Reduce(Factor::percent(50).unwrap());
        let fx = e.step(Event::Vetted {
            id: SPY1.into(),
            verdict: half,
        });
        assert_eq!(submits(&fx), [(SPY1, 12)]);
        let fx = e.step(Event::Vetted {
            id: SPY1.into(),
            verdict: Verdict::Approve,
        });
        assert!(submits(&fx).is_empty(), "a second answer is ignored");
        assert_eq!(of_kind(&fx, JournalKind::VerdictIgnored).len(), 1);
    }

    #[test]
    fn claude_reject_means_no_order() {
        let mut e = engine(true);
        breakout_day(&mut e, "SPY");
        let fx = e.step(Event::Vetted {
            id: SPY1.into(),
            verdict: Verdict::Reject,
        });
        assert!(submits(&fx).is_empty());
        assert_eq!(rejections(&fx), ["ClaudeRejected"]);
    }

    #[test]
    fn a_verdict_that_never_comes_times_out_and_frees_the_reservation() {
        let mut e = engine(true);
        breakout_day(&mut e, "SPY");
        let fx = e.step(Event::Tick(OPEN + 16 * 60 + VET_TIMEOUT_SECS));
        assert_eq!(
            of_kind(&fx, JournalKind::VetTimeout),
            ["no verdict in time"]
        );
        let fx = e.step(Event::Vetted {
            id: SPY1.into(),
            verdict: Verdict::Approve,
        });
        assert!(submits(&fx).is_empty(), "too late");
        e.step(start("2024-01-03", 1));
        assert!(
            !e.halted(),
            "a timed-out candidate isn't a leftover position"
        );
    }

    #[test]
    fn trend_filter_trades_only_with_the_market() {
        let filtered = || {
            let mut e = State::new(
                Limits::for_mode(Mode::Paper),
                Settings {
                    vetting: false,
                    orb: OrbParams::default(),
                    entry: Entry::Orb,
                    trend_filter: true,
                },
                risk::Allowed::Any,
            )
            .unwrap();
            e.step(start("2024-01-02", 0));
            e
        };
        let submitted = |fx: &[Effect]| fx.iter().any(|f| matches!(f, Effect::Submit { .. }));
        // SPY sliding from 500 to 498.5 over the same 16 minutes
        let spy_down: Vec<Bar> = (0..16)
            .map(|m| {
                let c = 500.0 - m as f64 * 0.1;
                bar(m, c + 0.2, c - 0.2, c, 5000)
            })
            .collect();
        let spy_up: Vec<Bar> = (0..16)
            .map(|m| {
                let c = 500.0 + m as f64 * 0.1;
                bar(m, c + 0.2, c - 0.2, c, 5000)
            })
            .collect();

        let mut e = filtered();
        feed(&mut e, "SPY", spy_down.clone());
        assert!(!submitted(&breakout_day(&mut e, "AAPL")), "market falling");

        let mut e = filtered();
        feed(&mut e, "SPY", spy_up.clone());
        assert!(submitted(&breakout_day(&mut e, "AAPL")), "market rising");

        let mut e = filtered();
        assert!(
            !submitted(&breakout_day(&mut e, "AAPL")),
            "no SPY bars: can't tell, so no trade"
        );

        // SPY's minute-15 bar crashes; whether it lands before or after AAPL's minute-15 breakout,
        // the filter reads SPY only through minute 14, so the result can't depend on arrival order
        let crash = bar(15, 501.6, 480.0, 480.0, 5000);
        let mut e = filtered();
        feed(&mut e, "SPY", spy_up[..15].to_vec());
        feed(&mut e, "SPY", [crash]);
        assert!(submitted(&breakout_day(&mut e, "AAPL")), "SPY bar first");
        let mut e = filtered();
        feed(&mut e, "SPY", spy_up[..15].to_vec());
        let fx = breakout_day(&mut e, "AAPL");
        feed(&mut e, "SPY", [crash]);
        assert!(submitted(&fx), "SPY bar after");

        // a later SPY minute already in (AAPL's bar is late) must not be read: no look-ahead
        let mut e = filtered();
        feed(&mut e, "SPY", spy_down[..15].to_vec());
        feed(&mut e, "SPY", [bar(16, 520.0, 519.0, 519.5, 5000)]);
        assert!(!submitted(&breakout_day(&mut e, "AAPL")), "future SPY bar");

        // SPY's feed stalls: one missing minute is tolerated, an older last bar means no trade
        let mut e = filtered();
        feed(&mut e, "SPY", spy_up[..14].to_vec());
        assert!(
            submitted(&breakout_day(&mut e, "AAPL")),
            "SPY minute 14 missing"
        );
        let mut e = filtered();
        feed(&mut e, "SPY", spy_up[..13].to_vec());
        assert!(
            !submitted(&breakout_day(&mut e, "AAPL")),
            "SPY stale since minute 12"
        );
    }

    #[test]
    fn trend_filter_gates_the_random_baseline_like_orb() {
        // AAPL drifts up all day; SPY drifts with `spy_step` per minute. One bar each per minute.
        let run = |trend_filter: bool, spy_step: f64| {
            let mut e = State::new(
                Limits::for_mode(Mode::Paper),
                Settings {
                    vetting: false,
                    orb: OrbParams::default(),
                    entry: Entry::Random { seed: 7 },
                    trend_filter,
                },
                risk::Allowed::Any,
            )
            .unwrap();
            e.step(start("2024-01-02", 0));
            let mut fx = Vec::new();
            for m in 0..330 {
                let s = 500.0 + m as f64 * spy_step;
                fx.extend(feed(&mut e, "SPY", [bar(m, s + 0.2, s - 0.2, s, 5000)]));
                let c = 100.0 + m as f64 * 0.01;
                fx.extend(feed(&mut e, "AAPL", [bar(m, c + 1.0, c - 1.0, c, 1000)]));
            }
            submits(&fx)
                .into_iter()
                .filter(|(id, _)| id.contains("AAPL"))
                .map(|(id, q)| (id.to_string(), q))
                .collect::<Vec<_>>()
        };
        let plain = run(false, 0.01);
        assert!(!plain.is_empty(), "the seed's minute falls inside the day");
        assert_eq!(run(true, 0.01), plain, "market up: same trades");
        assert!(run(true, -0.01).is_empty(), "market down: none");
    }

    #[test]
    fn reservations_count_while_claude_thinks() {
        // six symbols in different clusters break out in the same minute; the limit is 5 positions
        let mut e = engine(true);
        let names = ["SPY", "NVDA", "AAPL", "TSLA", "XYZ", "ABC"];
        for m in 0..15 {
            for s in names {
                feed(&mut e, s, [bar(m, 101.0, 100.0, 100.5, 1000)]);
            }
        }
        let fx: Vec<Effect> = names
            .iter()
            .flat_map(|s| feed(&mut e, s, [bar(15, 101.6, 100.9, 101.5, 3000)]))
            .collect();
        assert_eq!(
            fx.iter()
                .filter(|f| matches!(f, Effect::Vet { .. }))
                .count(),
            5
        );
        assert_eq!(rejections(&fx), ["MaxPositions"]);
    }

    #[test]
    fn earlier_trades_shrink_later_ones_through_buying_power() {
        let mut e = State::new(
            Limits::for_mode(Mode::Paper),
            Settings {
                vetting: false,
                orb: OrbParams::default(),
                entry: Entry::Orb,
                trend_filter: false,
            },
            risk::Allowed::Any,
        )
        .unwrap();
        e.step(Event::SessionStart {
            date: "d".into(),
            open: OPEN,
            close: CLOSE,
            buying_power: Usd::from_cents(300_000),
        });
        let fx = breakout_day(&mut e, "SPY");
        assert_eq!(submits(&fx), [("bmv-d-SPY-1", 24)]); // $2,436 committed of $3,000
        let fx = breakout_day(&mut e, "NVDA");
        assert_eq!(submits(&fx), [("bmv-d-NVDA-2", 5)]); // $564 left → 5 × $101.50
    }

    #[test]
    fn re_gating_after_the_verdict_uses_the_current_state() {
        let mut e = engine(true);
        breakout_day(&mut e, "SPY");
        e.step(Event::KillSwitch(true)); // flipped while Claude was thinking
        let fx = e.step(Event::Vetted {
            id: SPY1.into(),
            verdict: Verdict::Approve,
        });
        assert!(submits(&fx).is_empty());
        assert_eq!(rejections(&fx), ["KillSwitch"]);
    }

    #[test]
    fn one_candidate_per_symbol_even_after_a_rejection() {
        let mut e = engine(false);
        e.step(Event::KillSwitch(true));
        assert_eq!(rejections(&breakout_day(&mut e, "SPY")), ["KillSwitch"]);
        e.step(Event::KillSwitch(false));
        // a fresh engine view of a second SPY breakout must not trade (the guard, not the strategy)
        let c = Candidate::long(
            "SPY",
            price(10_150),
            price(10_000),
            price(10_450),
            String::new(),
        )
        .unwrap();
        let mut out = Vec::new();
        e.on_candidate(c, &mut out);
        assert_eq!(rejections(&out), ["duplicate candidate for symbol today"]);
    }

    // --- orders, fills and exits ---------------------------------------------------------------

    #[test]
    fn an_unfilled_entry_is_cancelled_once_after_one_bar() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        let fx = feed(
            &mut e,
            "SPY",
            [
                bar(16, 101.7, 101.2, 101.6, 900),
                bar(17, 101.7, 101.2, 101.6, 900),
            ],
        );
        assert_eq!(cancels(&fx), 1);
        let fx = e.step(Event::EntryClosed { id: SPY1.into() });
        assert_eq!(of_kind(&fx, JournalKind::EntryClosed), ["now Closed"]);
    }

    #[test]
    fn the_entry_ttl_fires_on_the_clock_without_bars() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        assert_eq!(cancels(&e.step(Event::Tick(OPEN + 17 * 60))), 1);
    }

    #[test]
    fn a_filled_entry_is_not_cancelled() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 24, 10_150));
        assert_eq!(
            cancels(&feed(&mut e, "SPY", [bar(16, 101.7, 101.2, 101.6, 900)])),
            0
        );
    }

    #[test]
    fn partial_fills_count_in_pnl_and_can_be_exited() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 10, 10_150));
        feed(&mut e, "SPY", [bar(16, 102.0, 101.4, 102.0, 900)]);
        assert_eq!(e.pnl_today(), Usd::from_cents(500)); // 10 × 50¢, still partly working
        // the target leg fills before the rest of the entry: book it, cancel the remainder (already sent by TTL)
        let fx = e.step(exit(SPY1, 10_250));
        assert_eq!(of_kind(&fx, JournalKind::Exit).len(), 1);
        assert_eq!(e.pnl_today(), Usd::from_cents(1_000));
        let fx = e.step(Event::EntryClosed { id: SPY1.into() });
        assert_eq!(
            of_kind(&fx, JournalKind::EntryClosed),
            ["ignored: trade is Closed"]
        );
    }

    #[test]
    fn an_exit_during_a_partial_fill_cancels_the_rest() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 10, 10_150));
        let fx = e.step(exit(SPY1, 10_000));
        assert_eq!(cancels(&fx), 1);
        assert_eq!(e.pnl_today(), Usd::from_cents(-1_500));
    }

    #[test]
    fn stale_fills_are_ignored() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 10, 10_150));
        let fx = e.step(fill(SPY1, 8, 10_150));
        assert_eq!(of_kind(&fx, JournalKind::FillIgnored).len(), 1);
        e.step(fill(SPY1, 24, 10_150));
        assert_eq!(
            of_kind(&e.step(fill(SPY1, 24, 10_150)), JournalKind::FillIgnored).len(),
            1
        );
    }

    #[test]
    fn anything_the_engine_cannot_reconcile_halts_and_flattens() {
        let cases: [fn(&mut State) -> Vec<Effect>; 5] = [
            |e| e.step(fill("nope", 5, 10_000)),
            |e| e.step(exit("nope", 10_000)),
            |e| {
                e.step(Event::EntryClosed { id: SPY1.into() });
                e.step(fill(SPY1, 5, 10_150)) // fill after the entry closed with nothing
            },
            |e| e.step(fill(SPY1, 30, 10_150)), // overfill
            |e| e.step(exit(SPY1, 10_000)),     // exit with nothing filled
        ];
        for case in cases {
            let mut e = engine(false);
            breakout_day(&mut e, "SPY");
            let fx = case(&mut e);
            assert_eq!(of_kind(&fx, JournalKind::Unexpected).len(), 1, "{fx:?}");
            assert!(e.halted());
            assert!(flattens(&fx) >= 1);
        }
    }

    // --- live loop: unresolved state, unexplained broker state, protection -------------------

    fn unresolved(key: &str) -> Event {
        Event::Unresolved {
            key: key.into(),
            why: "why".into(),
        }
    }

    #[test]
    fn live_trades_lists_working_and_open_with_their_mark() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY"); // SPY working, 24 ordered, mark = last close 101.50
        trade_and_exit(&mut e, "AAPL", 2, 10_300); // closed: not listed
        breakout_day(&mut e, "NVDA");
        let nvda = "bmv-2024-01-02-NVDA-3";
        e.step(fill(nvda, 24, 10_160)); // open
        e.marks.remove("NVDA"); // no bar → falls back to the fill avg
        let live = e.live_trades();
        assert_eq!(
            live,
            [
                LiveTrade {
                    id: nvda.into(),
                    symbol: "NVDA".into(),
                    working: false,
                    qty: 24,
                    filled: 24,
                    mark: price(10_160),
                },
                LiveTrade {
                    id: SPY1.into(),
                    symbol: "SPY".into(),
                    working: true,
                    qty: 24,
                    filled: 0,
                    mark: price(10_150),
                },
            ]
        );
        e.step(fill(SPY1, 10, 10_155));
        e.marks.clear(); // no bar, partial fill avg
        assert_eq!(e.live_trades()[1].filled, 10);
        assert_eq!(e.live_trades()[1].mark, price(10_155));
        // no bar and no fill → the candidate's entry
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.marks.clear();
        let entry = e.trades[SPY1].candidate.entry();
        assert_eq!(e.live_trades()[0].mark, entry);
    }

    #[test]
    fn unresolved_blocks_entries_without_halting_or_cancelling() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY"); // working entry
        let fx = e.step(unresolved("positions"));
        assert_eq!(of_kind(&fx, JournalKind::Unresolved), ["why"]);
        assert_eq!(cancels(&fx), 0);
        assert!(!e.halted());
        assert_eq!(rejections(&breakout_day(&mut e, "NVDA")), ["Halted"]);
        assert_eq!(
            of_kind(&e.step(unresolved("positions")), JournalKind::Unresolved).len(),
            0,
            "journals once"
        );
        let fx = e.step(Event::Resolved {
            key: "positions".into(),
        });
        assert_eq!(of_kind(&fx, JournalKind::Resolved).len(), 1);
        assert!(
            e.step(Event::Resolved {
                key: "positions".into()
            })
            .is_empty(),
            "resolving a missing key does nothing"
        );
        assert_eq!(submits(&breakout_day(&mut e, "AAPL")).len(), 1, "unblocked");
    }

    #[test]
    fn unresolved_survives_a_new_session() {
        let mut e = engine(false);
        e.step(unresolved("bug:x"));
        e.step(start("2024-01-03", 1));
        let mut bars = range_bars();
        bars.push(bar(15, 101.6, 100.9, 101.5, 3000));
        let fx = feed(
            &mut e,
            "SPY",
            bars.into_iter().map(|b| Bar {
                ts: b.ts + 86_400,
                ..b
            }),
        );
        assert_eq!(rejections(&fx), ["Halted"]);
    }

    #[test]
    fn unexplained_halts_and_flattens() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        let fx = e.step(Event::Unexplained {
            why: "short AAPL".into(),
        });
        assert_eq!(of_kind(&fx, JournalKind::Unexpected), ["short AAPL"]);
        assert!(e.halted());
        assert_eq!(cancels(&fx), 1);
        assert_eq!(flattens(&fx), 1);
    }

    #[test]
    fn closing_a_partial_entry_asks_for_protection_once() {
        let protects = |fx: &[Effect]| {
            fx.iter()
                .filter(|f| matches!(f, Effect::Protect { id } if id == SPY1))
                .count()
        };
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 10, 10_150));
        let fx = e.step(Event::EntryClosed { id: SPY1.into() });
        assert_eq!(protects(&fx), 1);
        assert_eq!(of_kind(&fx, JournalKind::Protect).len(), 1);
        assert_eq!(
            protects(&e.step(Event::EntryClosed { id: SPY1.into() })),
            0,
            "already open"
        );

        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        assert_eq!(protects(&e.step(Event::EntryClosed { id: SPY1.into() })), 0);

        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        let mut fx = e.step(fill(SPY1, 24, 10_150)); // full fill: bracket legs active
        fx.extend(e.step(Event::EntryClosed { id: SPY1.into() }));
        assert_eq!(protects(&fx), 0);
    }

    // --- halts, kill switch, end of day --------------------------------------------------------

    fn trade_and_exit(e: &mut State, symbol: &str, n: u32, exit_cents: i64) {
        breakout_day(e, symbol);
        let id = format!("bmv-2024-01-02-{symbol}-{n}");
        e.step(fill(&id, 24, 10_150));
        e.step(exit(&id, exit_cents));
    }

    #[test]
    fn three_losses_in_a_row_halt_the_day() {
        let mut e = engine(false);
        trade_and_exit(&mut e, "SPY", 1, 10_000);
        trade_and_exit(&mut e, "NVDA", 2, 10_000);
        assert!(!e.halted());
        trade_and_exit(&mut e, "AAPL", 3, 10_000);
        assert!(e.halted());
        assert_eq!(rejections(&breakout_day(&mut e, "TSLA")), ["Halted"]);
    }

    #[test]
    fn a_win_resets_the_loss_streak() {
        let mut e = engine(false);
        trade_and_exit(&mut e, "SPY", 1, 10_000);
        trade_and_exit(&mut e, "NVDA", 2, 10_000);
        trade_and_exit(&mut e, "AAPL", 3, 10_300);
        trade_and_exit(&mut e, "TSLA", 4, 10_000);
        assert!(!e.halted());
    }

    #[test]
    fn the_daily_loss_halt_stays_on_after_a_recovery() {
        let mut e = Engine::paper_with_loss(-50_016); // 24 shares × −$20.84
        assert!(e.halted());
        e.realized = Usd::ZERO; // P&L recovers
        assert_eq!(rejections(&breakout_day(&mut e, "TSLA")), ["Halted"]);
    }

    struct Engine;
    impl Engine {
        /// An engine that just booked `cents` of realized loss through a real exit.
        fn paper_with_loss(cents: i64) -> State {
            let mut e = engine(false);
            breakout_day(&mut e, "SPY");
            e.step(fill(SPY1, 24, 10_150));
            e.step(exit(SPY1, 10_150 + cents / 24));
            e
        }
    }

    #[test]
    fn kill_switch_blocks_entries_and_cancels_working_orders() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        let fx = e.step(Event::KillSwitch(true));
        assert_eq!(cancels(&fx), 1);
        assert_eq!(rejections(&breakout_day(&mut e, "NVDA")), ["KillSwitch"]);
    }

    #[test]
    fn a_halt_cancels_working_orders() {
        let mut e = engine(false);
        trade_and_exit(&mut e, "SPY", 1, 10_000);
        trade_and_exit(&mut e, "NVDA", 2, 10_000);
        breakout_day(&mut e, "AAPL"); // working entry #3
        let fx = {
            let mut fx = e.step(fill("bmv-2024-01-02-TSLA-0", 1, 1)); // unexpected → halt
            fx.retain(|f| matches!(f, Effect::CancelEntry { .. }));
            fx
        };
        assert_eq!(
            fx,
            [Effect::CancelEntry {
                id: "bmv-2024-01-02-AAPL-3".into()
            }]
        );
    }

    #[test]
    fn no_entries_after_the_cutoff_and_pending_candidates_are_dropped() {
        let mut e = engine(true);
        breakout_day(&mut e, "SPY"); // awaiting a verdict
        let late = CLOSE - ENTRY_CUTOFF_SECS;
        let mut bars = range_bars();
        bars.extend((16..((late - OPEN) / 60)).map(|m| bar(m, 100.9, 100.1, 100.5, 1000)));
        bars.push(Bar {
            ts: late,
            ..bar(0, 101.6, 100.9, 101.5, 9000)
        });
        let fx = feed(&mut e, "NVDA", bars);
        assert_eq!(rejections(&fx), ["OutsideWindow"]);
        assert!(
            of_kind(&fx, JournalKind::VetTimeout).len() == 1,
            "SPY's pending candidate dropped"
        );
    }

    #[test]
    fn flatten_fires_on_the_clock_and_repeats_until_flat() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 24, 10_150));
        let t = CLOSE - FLATTEN_SECS;
        assert_eq!(flattens(&e.step(Event::Tick(t))), 1, "no bars needed");
        assert_eq!(flattens(&e.step(Event::Tick(t + 30))), 0);
        assert_eq!(
            flattens(&e.step(Event::Tick(t + FLATTEN_RETRY_SECS))),
            1,
            "still holding → again"
        );
        e.step(exit(SPY1, 10_150));
        assert_eq!(
            flattens(&e.step(Event::Tick(t + 2 * FLATTEN_RETRY_SECS))),
            0,
            "flat → stop"
        );
        assert!(!e.halted());
    }

    #[test]
    fn still_holding_after_the_close_halts_loudly() {
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 24, 10_150));
        let fx = e.step(Event::Tick(CLOSE));
        assert!(e.halted());
        assert_eq!(
            of_kind(&fx, JournalKind::Halt),
            ["still holding positions after the close"]
        );
        assert!(
            of_kind(&e.step(Event::Tick(CLOSE + 60)), JournalKind::Halt).is_empty(),
            "alarm once"
        );
    }

    #[test]
    fn kill_switch_blocks_new_entries() {
        let mut e = engine(false);
        e.step(Event::KillSwitch(true));
        assert_eq!(rejections(&breakout_day(&mut e, "SPY")), ["KillSwitch"]);
    }

    // --- data ----------------------------------------------------------------------------------

    #[test]
    fn bars_outside_the_session_or_out_of_order_are_ignored() {
        let mut e = engine(false);
        let fx = feed(
            &mut e,
            "SPY",
            [
                bar(-1, 101.0, 100.0, 100.5, 1000),
                bar(400, 101.0, 100.0, 100.5, 1000),
            ],
        );
        assert_eq!(of_kind(&fx, JournalKind::BarIgnored).len(), 2);
        feed(&mut e, "SPY", [bar(3, 101.0, 100.0, 100.5, 1000)]);
        assert_eq!(
            of_kind(
                &feed(&mut e, "SPY", [bar(2, 101.0, 100.0, 100.5, 1000)]),
                JournalKind::BarIgnored
            )
            .len(),
            1
        );
    }

    #[test]
    fn bad_bars_are_rejected_and_halt_if_the_symbol_has_a_position() {
        let mut e = engine(false);
        let fx = feed(&mut e, "SPY", [bar(1, 101.0, 100.0, f64::NAN, 1000)]);
        assert_eq!(of_kind(&fx, JournalKind::BarRejected).len(), 1);
        assert!(!e.halted(), "no position → just skip the bar");
        let mut e = engine(false);
        breakout_day(&mut e, "SPY");
        e.step(fill(SPY1, 24, 10_150));
        feed(&mut e, "SPY", [bar(16, 101.0, 102.0, 101.5, 900)]); // low above high
        assert!(e.halted());
    }

    #[test]
    fn the_same_events_always_give_the_same_effects() {
        let run = || {
            let mut e = engine(true);
            let mut fx = breakout_day(&mut e, "SPY");
            fx.extend(breakout_day(&mut e, "NVDA"));
            fx.extend(e.step(Event::Vetted {
                id: SPY1.into(),
                verdict: Verdict::Approve,
            }));
            fx.extend(e.step(Event::Tick(CLOSE)));
            format!("{fx:?}")
        };
        assert_eq!(run(), run());
    }
}
