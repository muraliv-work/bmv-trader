//! The P3 paper run loop (DESIGN §4, §8, §12): blocking and single-threaded. Each 5 s iteration
//! reads the kill switch, retries what failed, reconciles broker state into engine events, feeds
//! closed bars and steps the clock. Effects become broker calls here; the decisions stay pure and
//! unit-tested (`order_events`, `position_events`, the startup checks). Unknown means stop: any
//! broker outcome we can't confirm is an `Unresolved` key, and the engine takes no new entries
//! while one is set.

use crate::broker::{
    Account, Alpaca, Api, Bracket, BracketError, Cancel, Exits, Order, Position, SubmitError,
};
use crate::config::Mode;
use crate::engine::{Effect, Event, LiveTrade, Settings, State};
use crate::history::{self, CalendarRow, Session};
use crate::market_data::{self, Feed};
use crate::money::{Price, Qty, Usd};
use crate::risk::{Limits, OrderIntent};
use crate::safety::{self, KillSwitch};
use crate::strategy::Bar;
use crate::vetting::Verdict;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The broker calls the loop makes. Alpaca implements it; tests use a fake.
pub trait Broker {
    fn submit_bracket(&self, b: Bracket) -> Result<(Order, Exits), BracketError>;
    fn submit_oco(&self, x: &Exits, entry: &Order) -> Result<Order, SubmitError>;
    fn cancel_by_client_id(&self, id: &str) -> Result<Cancel, String>;
    fn close_position(&self, symbol: &str) -> Result<(), String>;
    fn close_all(&self) -> Result<(), String>;
    fn positions(&self) -> Result<Vec<Position>, String>;
    fn order_by_client_id(&self, id: &str) -> Result<Option<Order>, String>;
    fn open_orders(&self) -> Result<Vec<Order>, String>;
    fn bars(
        &self,
        symbols: &[String],
        from: i64,
        to: i64,
    ) -> Result<BTreeMap<String, Vec<Bar>>, String>;
}

impl Broker for Alpaca {
    fn submit_bracket(&self, b: Bracket) -> Result<(Order, Exits), BracketError> {
        Alpaca::submit_bracket(self, b)
    }
    fn submit_oco(&self, x: &Exits, entry: &Order) -> Result<Order, SubmitError> {
        Alpaca::submit_oco(self, x, entry)
    }
    fn cancel_by_client_id(&self, id: &str) -> Result<Cancel, String> {
        Alpaca::cancel_by_client_id(self, id)
    }
    fn close_position(&self, symbol: &str) -> Result<(), String> {
        Alpaca::close_position(self, symbol)
    }
    fn close_all(&self) -> Result<(), String> {
        Alpaca::close_all(self)
    }
    fn positions(&self) -> Result<Vec<Position>, String> {
        Alpaca::positions(self)
    }
    fn order_by_client_id(&self, id: &str) -> Result<Option<Order>, String> {
        Alpaca::order_by_client_id(self, id)
    }
    fn open_orders(&self) -> Result<Vec<Order>, String> {
        open_orders(self)
    }
    fn bars(
        &self,
        symbols: &[String],
        from: i64,
        to: i64,
    ) -> Result<BTreeMap<String, Vec<Bar>>, String> {
        market_data::fetch(self, symbols, from, to)
    }
}

// shortcut: JSONL instead of the SQLite events table (DESIGN §11), move to SQLite with reports.
pub fn journal_path(home: &Path) -> PathBuf {
    home.join("data").join("journal.jsonl")
}

/// The unix time of `run`'s latest tick, so monitors can tell a quiet engine from a dead one.
pub fn heartbeat_path(home: &Path) -> PathBuf {
    home.join("data").join("heartbeat")
}

/// Append-only, one fsynced JSON line per record.
pub struct Journal {
    file: std::fs::File,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Journal, String> {
        let err = |e: std::io::Error| format!("journal {}: {e}", path.display());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(err)?;
        }
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(err)?;
        Ok(Journal { file })
    }

    pub fn write(&mut self, ts: i64, kind: &str, id: &str, detail: &str) -> Result<(), String> {
        let mut line =
            serde_json::json!({"ts": ts, "kind": kind, "id": id, "detail": detail}).to_string();
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .map_err(|e| format!("journal write: {e}"))
    }
}

/// True if the journal has a line at or after `open`: a run already started this session.
/// shortcut: a restart is refused instead of replaying today's journal into the engine (trade
/// count, used symbols, realized P&L, halts, the id sequence), replay it when the journal moves to SQLite.
pub fn ran_since(path: &Path, open: i64) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("journal {}: {e}", path.display())),
    };
    for (n, line) in text.lines().enumerate() {
        let ts = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v["ts"].as_i64())
            .ok_or_else(|| format!("journal {} line {}: unreadable", path.display(), n + 1))?;
        if ts >= open {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Entry statuses that mean the order is gone. [verify statuses]
pub const ENTRY_CLOSED: [&str; 3] = ["canceled", "expired", "rejected"];
/// Entry statuses we understand that aren't gone. Anything else is unresolved. [verify statuses]
pub const ENTRY_LIVE: [&str; 10] = [
    "new",
    "accepted",
    "pending_new",
    "accepted_for_bidding",
    "partially_filled",
    "pending_cancel",
    "filled",
    "done_for_day",
    "held",
    "calculated",
];

fn unresolved(key: &str, why: impl Into<String>) -> Event {
    Event::Unresolved {
        key: key.to_string(),
        why: why.into(),
    }
}

fn resolved(key: &str) -> Event {
    Event::Resolved {
        key: key.to_string(),
    }
}

/// What the broker's view of one live trade means for the engine. `entry` is its order, `oco`
/// the "{id}-x" OCO if one was placed, `position` this symbol's qty (None: positions() failed).
pub fn order_events(
    t: &LiveTrade,
    entry: &Order,
    oco: Option<&Order>,
    position: Option<i64>,
) -> Vec<Event> {
    let unexplained = |why: String| {
        vec![Event::Unexplained {
            why: format!("{}: {why}", t.id),
        }]
    };
    if entry.client_order_id != t.id || entry.symbol != t.symbol || entry.side != "buy" {
        return unexplained(format!(
            "broker order is {} {} {}",
            entry.client_order_id, entry.side, entry.symbol
        ));
    }
    if t.working && entry.qty != t.qty {
        return unexplained(format!("ordered {}, broker shows {}", t.qty, entry.qty));
    }
    let status = entry.status.as_str();
    let closed = ENTRY_CLOSED.contains(&status);
    if !closed && !ENTRY_LIVE.contains(&status) {
        return vec![unresolved(&t.id, format!("status {status}"))];
    }
    let mut out = Vec::new();
    if entry.filled_qty > t.filled {
        match (Qty::new(entry.filled_qty), entry.filled_avg_price) {
            (Some(cum_qty), Some(avg_price)) => out.push(Event::EntryFilled {
                id: t.id.clone(),
                cum_qty,
                avg_price,
            }),
            _ => {
                return vec![unresolved(
                    &t.id,
                    format!("{} filled with no average price", entry.filled_qty),
                )];
            }
        }
    }
    if t.working && closed {
        out.push(Event::EntryClosed { id: t.id.clone() });
    }
    fn legs(o: &Order) -> impl Iterator<Item = &Order> {
        o.legs.iter().flatten()
    }
    let mut sells: Vec<&Order> = legs(entry).collect();
    if let Some(x) = oco {
        sells.push(x);
        sells.extend(legs(x));
    }
    let (mut sold, mut value) = (0i64, 0i64);
    for o in sells.into_iter().filter(|o| o.side == "sell") {
        if let Some(p) = o.filled_avg_price {
            sold += i64::from(o.filled_qty);
            value += p.cents() * i64::from(o.filled_qty);
        }
    }
    let held = i64::from(entry.filled_qty);
    if held > 0 && sold >= held {
        out.extend(Price::from_cents(value / sold).map(|price| Event::Exited {
            id: t.id.clone(),
            price,
        }));
    } else if position == Some(0) && t.filled > 0 {
        // shortcut: a market close (flatten / close_position) books the last bar close, not the real fill price. Read fill activities in a later P3 step.
        out.push(Event::Exited {
            id: t.id.clone(),
            price: t.mark,
        });
    }
    out
}

/// An Unexplained for each position the engine's trades can't account for.
pub fn position_events(trades: &[LiveTrade], positions: &[Position]) -> Vec<Event> {
    positions
        .iter()
        .filter_map(|p| {
            let most = trades
                .iter()
                .filter(|t| t.symbol == p.symbol)
                .map(|t| i64::from(t.qty))
                .reduce(|a, b| a + b);
            let why = match most {
                _ if p.qty < 0 => format!("short position {} {}", p.qty, p.symbol),
                None => format!("position {} {} with no trade", p.qty, p.symbol),
                Some(m) if p.qty > m => format!("position {} {} over {m}", p.qty, p.symbol),
                Some(_) => return None,
            };
            Some(Event::Unexplained { why })
        })
        .collect()
}

/// Turns engine Effects into broker calls and broker outcomes into engine events.
pub struct Executor<B: Broker> {
    broker: B,
    journal: Journal,
    /// Placed brackets, for Protect.
    exits: BTreeMap<String, Exits>,
    /// Submit outcome unknown: kept, never resent.
    unknown: BTreeMap<String, Bracket>,
    /// Entry ids whose "{id}-x" OCO is placed (polled).
    oco: BTreeSet<String>,
    retry_cancel: BTreeSet<String>,
    retry_protect: BTreeSet<String>,
    retry_flatten: bool,
    /// A FlattenAll ran today, so Protect is skipped.
    flattened: bool,
    journal_failed: bool,
    /// Protect fell back to close_position: id -> (symbol, when). Unresolved until the position is gone.
    closing: BTreeMap<String, (String, i64)>,
    /// Trades seen flat once while holding shares: a second flat poll confirms (positions() can lag a fill).
    flat_once: BTreeSet<String>,
    /// Trades that just exited, with when: their shares may still show in a lagging positions().
    recent: Vec<(i64, LiveTrade)>,
}

/// How long close_position may take to clear a position before it is Unexplained.
pub const CLOSE_CONFIRM_SECS: i64 = 30;
/// How long a just-exited trade still explains its symbol's position.
pub const EXIT_GRACE_SECS: i64 = 15;

impl<B: Broker> Executor<B> {
    pub fn new(broker: B, journal: Journal) -> Self {
        Executor {
            broker,
            journal,
            exits: BTreeMap::new(),
            unknown: BTreeMap::new(),
            oco: BTreeSet::new(),
            retry_cancel: BTreeSet::new(),
            retry_protect: BTreeSet::new(),
            retry_flatten: false,
            flattened: false,
            journal_failed: false,
            closing: BTreeMap::new(),
            flat_once: BTreeSet::new(),
            recent: Vec::new(),
        }
    }

    pub fn broker(&self) -> &B {
        &self.broker
    }

    /// One journal line; a failure blocks entries until a write succeeds again.
    fn log(&mut self, now: i64, kind: &str, id: &str, detail: &str, out: &mut Vec<Event>) {
        match self.journal.write(now, kind, id, detail) {
            Ok(()) if self.journal_failed => {
                self.journal_failed = false;
                out.push(resolved("journal"));
            }
            Ok(()) => {}
            Err(e) => {
                eprintln!("error: {e} ({kind} {id} {detail})");
                self.journal_failed = true;
                out.push(unresolved("journal", e));
            }
        }
    }

    pub fn apply(&mut self, now: i64, effects: Vec<Effect>) -> Vec<Event> {
        let mut out = Vec::new();
        for effect in effects {
            match effect {
                Effect::Journal { kind, id, detail } => {
                    self.log(now, &format!("{kind:?}"), &id, &detail, &mut out)
                }
                Effect::Submit { id, order } => self.submit(now, id, order, &mut out),
                Effect::CancelEntry { id } => self.cancel(now, &id, &mut out),
                Effect::Protect { id } if self.flattened => {
                    self.log(now, "broker.oco", &id, "skipped: flattened today", &mut out)
                }
                Effect::Protect { id } => self.protect(now, &id, &mut out),
                Effect::FlattenAll => self.flatten_all(now, &mut out),
                // shortcut: vetting is P4, every vetted candidate is rejected until then.
                Effect::Vet { id, .. } => out.push(Event::Vetted {
                    id,
                    verdict: Verdict::Reject,
                }),
            }
        }
        out
    }

    fn submit(&mut self, now: i64, id: String, order: OrderIntent, out: &mut Vec<Event>) {
        if self.journal_failed {
            out.push(Event::EntryClosed { id: id.clone() });
            return self.log(now, "broker.submit", &id, "not sent: journal failing", out);
        }
        let detail = match self.broker.submit_bracket(Bracket::new(id.clone(), order)) {
            Ok((o, x)) => {
                self.exits.insert(id.clone(), x);
                format!("placed: {}", o.status)
            }
            Err(BracketError::Failed(SubmitError::Rejected(m))) => {
                out.push(Event::EntryClosed { id: id.clone() });
                format!("rejected: {m}")
            }
            Err(BracketError::Failed(SubmitError::Invalid(m))) => {
                out.push(Event::EntryClosed { id: id.clone() });
                // never resolved: no entries for the rest of the day
                out.push(unresolved(&format!("bug:{id}"), m.clone()));
                format!("invalid: {m}")
            }
            Err(BracketError::Failed(SubmitError::Unknown(m))) => {
                out.push(unresolved(&id, m.clone()));
                format!("unknown: {m}")
            }
            Err(BracketError::Unknown(m, b)) => {
                self.unknown.insert(id.clone(), b);
                out.push(unresolved(&id, m.clone()));
                format!("unknown: {m}")
            }
        };
        self.log(now, "broker.submit", &id, &detail, out);
    }

    fn cancel(&mut self, now: i64, id: &str, out: &mut Vec<Event>) {
        let key = format!("cancel:{id}");
        let detail = match self.broker.cancel_by_client_id(id) {
            Ok(c) => {
                if self.retry_cancel.remove(id) {
                    out.push(resolved(&key));
                }
                if c == Cancel::NotFound {
                    out.push(unresolved(id, "cancel: order not found"));
                }
                format!("{c:?}")
            }
            Err(e) => {
                self.retry_cancel.insert(id.to_string());
                out.push(unresolved(&key, e.clone()));
                format!("error: {e}")
            }
        };
        self.log(now, "broker.cancel", id, &detail, out);
    }

    fn oco_placed(&mut self, id: &str, out: &mut Vec<Event>) {
        self.retry_protect.remove(id);
        self.oco.insert(id.to_string());
        out.push(resolved(&format!("{id}-x")));
    }

    fn protect(&mut self, now: i64, id: &str, out: &mut Vec<Event>) {
        let key = format!("{id}-x");
        if !self.exits.contains_key(id) {
            self.retry_protect.remove(id);
            out.push(Event::Unexplained {
                why: format!("{id}: a fill to protect but no placed bracket"),
            });
            return;
        }
        if self.retry_protect.contains(id) {
            // An OCO that landed is never resent.
            match self.broker.order_by_client_id(&key) {
                Ok(Some(_)) => {
                    self.oco_placed(id, out);
                    return self.log(now, "broker.oco", id, "found placed", out);
                }
                Ok(None) => {}
                Err(e) => return out.push(unresolved(&key, e)),
            }
        }
        let entry = match self.broker.order_by_client_id(id) {
            Ok(Some(o)) => o,
            other => {
                let e = other
                    .err()
                    .unwrap_or_else(|| "entry order not found".to_string());
                self.retry_protect.insert(id.to_string());
                return out.push(unresolved(&key, e));
            }
        };
        let detail = match self.broker.submit_oco(&self.exits[id], &entry) {
            Ok(o) => {
                self.oco_placed(id, out);
                format!("placed: {}", o.status)
            }
            Err(SubmitError::Rejected(m)) => {
                self.retry_protect.remove(id);
                let closed = self.broker.close_position(&entry.symbol);
                let detail = match &closed {
                    Ok(()) => {
                        // Accepted is not filled: entries stay blocked until the position is gone.
                        self.closing
                            .insert(id.to_string(), (entry.symbol.clone(), now));
                        out.push(unresolved(&key, "OCO refused, close sent"));
                        "accepted".to_string()
                    }
                    Err(e) => {
                        out.push(Event::Unexplained {
                            why: format!(
                                "{id}: OCO refused and close {} failed: {e}",
                                entry.symbol
                            ),
                        });
                        format!("error: {e}")
                    }
                };
                self.log(now, "broker.close", id, &detail, out);
                format!("rejected: {m}")
            }
            Err(SubmitError::Invalid(m)) => {
                self.retry_protect.remove(id);
                out.push(Event::Unexplained {
                    why: format!("{id}: OCO invalid: {m}"),
                });
                format!("invalid: {m}")
            }
            Err(SubmitError::Unknown(m)) => {
                self.retry_protect.insert(id.to_string());
                out.push(unresolved(&key, m.clone()));
                format!("unknown: {m}")
            }
        };
        self.log(now, "broker.oco", id, &detail, out);
    }

    fn flatten_all(&mut self, now: i64, out: &mut Vec<Event>) {
        self.flattened = true;
        let detail = match self.broker.close_all() {
            Ok(()) => {
                self.retry_flatten = false;
                out.push(resolved("flatten"));
                "accepted".to_string()
            }
            Err(e) => {
                self.retry_flatten = true;
                out.push(unresolved("flatten", e.clone()));
                format!("error: {e}")
            }
        };
        self.log(now, "broker.close_all", "", &detail, out);
    }

    /// positions() first, then the orders: positions can only lag the orders, never lead them.
    pub fn reconcile(&mut self, now: i64, trades: &[LiveTrade]) -> Vec<Event> {
        let mut out = Vec::new();
        let held: Option<BTreeMap<String, i64>> = match self.broker.positions() {
            Ok(ps) => {
                out.push(resolved("positions"));
                self.recent.retain(|(at, _)| now - at <= EXIT_GRACE_SECS);
                let explain: Vec<LiveTrade> = trades
                    .iter()
                    .cloned()
                    .chain(self.recent.iter().map(|(_, t)| t.clone()))
                    .collect();
                let mut evs = position_events(&explain, &ps);
                for (id, (symbol, since)) in std::mem::take(&mut self.closing) {
                    if !ps.iter().any(|p| p.symbol == symbol && p.qty != 0) {
                        evs.push(resolved(&format!("{id}-x")));
                    } else if now - since >= CLOSE_CONFIRM_SECS {
                        evs.push(Event::Unexplained {
                            why: format!(
                                "{id}: {symbol} still held {CLOSE_CONFIRM_SECS}s after close"
                            ),
                        });
                    } else {
                        self.closing.insert(id, (symbol, since));
                    }
                }
                if !evs.is_empty() {
                    self.log(
                        now,
                        "broker.poll",
                        "positions",
                        &format!("{evs:?}"),
                        &mut out,
                    );
                }
                out.extend(evs);
                Some(ps.into_iter().map(|p| (p.symbol, p.qty)).collect())
            }
            Err(e) => {
                self.log(now, "broker.poll", "positions", &e, &mut out);
                out.push(unresolved("positions", e));
                None
            }
        };
        for t in trades {
            let entry = match self.broker.order_by_client_id(&t.id) {
                Ok(Some(o)) => o,
                Ok(None) => {
                    // not proof it's absent (S11)
                    self.log(now, "broker.poll", &t.id, "order not found", &mut out);
                    out.push(unresolved(&t.id, "order not found"));
                    continue;
                }
                Err(e) => {
                    self.log(now, "broker.poll", &t.id, &e, &mut out);
                    out.push(unresolved(&t.id, e));
                    continue;
                }
            };
            if let Some(b) = self.unknown.remove(&t.id) {
                self.exits.insert(t.id.clone(), b.into_exits());
                self.log(
                    now,
                    "broker.poll",
                    &t.id,
                    "unknown submit found placed",
                    &mut out,
                );
            }
            let key = format!("{}-x", t.id);
            let oco = if self.oco.contains(&t.id) {
                match self.broker.order_by_client_id(&key) {
                    Ok(Some(o)) => {
                        out.push(resolved(&key));
                        Some(o)
                    }
                    Ok(None) => {
                        self.log(now, "broker.poll", &key, "OCO not found", &mut out);
                        out.push(unresolved(&key, "OCO not found"));
                        None
                    }
                    Err(e) => {
                        self.log(now, "broker.poll", &key, &e, &mut out);
                        out.push(unresolved(&key, e));
                        None
                    }
                }
            } else {
                None
            };
            let mut position = held
                .as_ref()
                .map(|h| h.get(&t.symbol).copied().unwrap_or(0));
            if position == Some(0) && t.filled > 0 {
                if self.flat_once.insert(t.id.clone()) {
                    position = None; // first flat reading: unconfirmed
                }
            } else {
                self.flat_once.remove(&t.id);
            }
            let evs = order_events(t, &entry, oco.as_ref(), position);
            if evs.iter().any(|e| matches!(e, Event::Exited { .. })) {
                self.flat_once.remove(&t.id);
                self.recent.push((now, t.clone()));
            }
            if !evs
                .iter()
                .any(|e| matches!(e, Event::Unresolved { key, .. } if *key == t.id))
            {
                out.push(resolved(&t.id));
            }
            if !evs.is_empty() {
                self.log(now, "broker.poll", &t.id, &format!("{evs:?}"), &mut out);
            }
            out.extend(evs);
        }
        out
    }

    /// Retries pending cancels, Protects and flattens.
    pub fn retry(&mut self, now: i64) -> Vec<Event> {
        let mut out = Vec::new();
        for id in self.retry_cancel.clone() {
            self.cancel(now, &id, &mut out);
        }
        if self.flattened {
            self.retry_protect.clear();
        }
        for id in self.retry_protect.clone() {
            self.protect(now, &id, &mut out);
        }
        if self.retry_flatten {
            self.flatten_all(now, &mut out);
        }
        out
    }
}

/// A feedback loop between the engine and the executor, not a busy minute.
pub const MAX_EVENTS_PER_DRIVE: usize = 10_000;

/// Feeds `events` and their follow-ups through the engine and the executor, FIFO like
/// backtest::Sim::drive. Returns every event fed.
pub fn drive<B: Broker>(
    engine: &mut State,
    ex: &mut Executor<B>,
    now: i64,
    events: Vec<Event>,
) -> Result<Vec<Event>, String> {
    let mut queue = VecDeque::from(events);
    let mut fed = Vec::new();
    while let Some(event) = queue.pop_front() {
        if fed.len() >= MAX_EVENTS_PER_DRIVE {
            return Err(format!(
                "over {MAX_EVENTS_PER_DRIVE} events in one step: engine/executor feedback loop"
            ));
        }
        let effects = engine.step(event.clone());
        fed.push(event);
        queue.extend(ex.apply(now, effects));
    }
    Ok(fed)
}

const MARKET_DATA: &str = "market-data";

pub struct Runner<B: Broker> {
    pub engine: State,
    pub ex: Executor<B>,
    pub feed: Feed,
    kill: bool,
    close: i64,
}

impl<B: Broker> Runner<B> {
    /// Starts the session with market data unresolved, so the catch-up bars at a mid-session
    /// start can't open trades.
    pub fn new(
        mut engine: State,
        mut ex: Executor<B>,
        feed: Feed,
        session: &Session,
        buying_power: Usd,
    ) -> Result<Self, String> {
        let start = Event::SessionStart {
            date: session.date.clone(),
            open: session.open,
            close: session.close,
            buying_power,
        };
        drive(
            &mut engine,
            &mut ex,
            session.open,
            vec![start, unresolved(MARKET_DATA, "no bars yet")],
        )?;
        if engine.halted() {
            return Err(format!("the engine refused session {session:?}"));
        }
        Ok(Runner {
            engine,
            ex,
            feed,
            kill: false,
            close: session.close,
        })
    }

    fn drive(&mut self, now: i64, events: Vec<Event>) -> Result<Vec<Event>, String> {
        drive(&mut self.engine, &mut self.ex, now, events)
    }

    /// One loop iteration at `now` with this kill-switch reading. Returns every event fed.
    pub fn tick(&mut self, now: i64, kill: &KillSwitch) -> Result<Vec<Event>, String> {
        let mut fed = Vec::new();
        let engaged = *kill != KillSwitch::Clear;
        if engaged != self.kill {
            self.kill = engaged;
            fed.extend(self.drive(now, vec![Event::KillSwitch(engaged)])?);
        }
        let evs = self.ex.retry(now);
        fed.extend(self.drive(now, evs)?);
        let trades = self.engine.live_trades();
        let evs = self.ex.reconcile(now, &trades);
        fed.extend(self.drive(now, evs)?);

        let mut evs = Vec::new();
        let mut fetch_failed = false;
        if let Some((from, to)) = self.feed.window(now) {
            match self.ex.broker().bars(self.feed.symbols(), from, to) {
                Ok(b) => evs.extend(
                    self.feed
                        .accept(b, to)
                        .into_iter()
                        .map(|(symbol, bar)| Event::Bar { symbol, bar }),
                ),
                Err(e) => {
                    fetch_failed = true;
                    evs.push(unresolved(MARKET_DATA, format!("bars: {e}")));
                }
            }
        }
        // After the bars, so the ones fed in this tick are judged with the data still unresolved.
        if self.feed.stale(now) {
            evs.push(unresolved(MARKET_DATA, "no fresh bars"));
        } else if !fetch_failed {
            evs.push(resolved(MARKET_DATA));
        }
        evs.push(Event::Tick(now));
        fed.extend(self.drive(now, evs)?);
        Ok(fed)
    }

    /// Done once the session is over and nothing is live; an error if still holding 15 minutes after the close.
    pub fn finished(&self, now: i64) -> Result<bool, String> {
        let live = self.engine.live_trades();
        if !live.is_empty() && now >= self.close + 900 {
            let ids: Vec<&str> = live.iter().map(|t| t.id.as_str()).collect();
            return Err(format!(
                "still live 15 minutes after the close: {}",
                ids.join(", ")
            ));
        }
        Ok(live.is_empty() && now >= self.close + 300)
    }
}

pub const MAX_SKEW_MS: i64 = 2_000;
pub const LOOP_SECS: u64 = 5;

pub fn refuse_live(mode: Mode) -> Result<(), String> {
    match mode {
        Mode::Paper => Ok(()),
        Mode::Live => Err("live trading is P5; run is paper only".into()),
    }
}

/// The 1-minute bars and the time rules need our clock within 2 s of Alpaca's.
pub fn check_skew(server_rfc3339: &str, local_ms: i64) -> Result<(), String> {
    let server = chrono::DateTime::parse_from_rfc3339(server_rfc3339)
        .map_err(|e| format!("Alpaca clock {server_rfc3339:?}: {e}"))?
        .timestamp_millis();
    let skew = local_ms - server;
    if skew.abs() > MAX_SKEW_MS {
        return Err(format!(
            "local clock is {skew} ms off Alpaca's (max {MAX_SKEW_MS}): fix the system time"
        ));
    }
    Ok(())
}

/// Today's session: exactly one calendar row dated `today`.
pub fn pick_session(rows: &[CalendarRow], today: &str) -> Result<Session, String> {
    let todays: Vec<CalendarRow> = rows.iter().filter(|r| r.date == today).cloned().collect();
    match todays.len() {
        1 => history::parse_calendar(&todays)?
            .pop()
            .ok_or_else(|| "calendar parse returned nothing".to_string()),
        0 => Err(format!("{today} is not a trading day")),
        n => Err(format!("calendar has {n} rows for {today}")),
    }
}

/// The engine starts with no memory, so the account must start flat.
pub fn require_flat(positions: &[Position], open_orders: &[Order]) -> Result<(), String> {
    if positions.is_empty() && open_orders.is_empty() {
        return Ok(());
    }
    let p: Vec<String> = positions
        .iter()
        .map(|p| format!("{} {}", p.qty, p.symbol))
        .collect();
    let o: Vec<&str> = open_orders
        .iter()
        .map(|o| o.client_order_id.as_str())
        .collect();
    Err(format!(
        "account not flat (positions [{}], open orders [{}]): run `bmv-trader flatten` first",
        p.join(", "),
        o.join(", ")
    ))
}

/// The ticker allow-list (DESIGN §9): `config/allowlist.txt`, one symbol per line, `#` starts a
/// comment. The owner edits it; a missing, empty or malformed list means no trading.
pub fn allowlist(home: &Path) -> Result<BTreeSet<String>, String> {
    let path = home.join("config").join("allowlist.txt");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("ticker allow-list {}: {e}", path.display()))?;
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let name = line
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_uppercase();
        if name.is_empty() {
            continue;
        }
        if !history::valid_symbol(&name) {
            return Err(format!("ticker allow-list: {name:?} is not a ticker"));
        }
        names.insert(name);
    }
    if names.is_empty() {
        return Err(format!("ticker allow-list {} is empty", path.display()));
    }
    Ok(names)
}

/// One owner drill position the backstop leaves alone (R13 step 3a): exactly this many shares,
/// long, plus the owner's own sell orders on it (its GTC stop). `data/drill.json`, a JSON list.
#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Drill {
    pub symbol: String,
    pub qty: i64,
}

/// The drill file. Missing means no drill. Anything malformed is an error, and the backstop then
/// flattens everything: a bad file never makes it skip a position.
pub fn drills(home: &Path) -> Result<Vec<Drill>, String> {
    let path = home.join("data").join("drill.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("drill file {}: {e}", path.display())),
    };
    let list: Vec<Drill> =
        serde_json::from_str(&text).map_err(|e| format!("drill file {}: {e}", path.display()))?;
    let mut seen = BTreeSet::new();
    for d in &list {
        if !history::valid_symbol(&d.symbol) || d.qty <= 0 || !seen.insert(d.symbol.as_str()) {
            return Err(format!(
                "drill file: {} {} must be a ticker, listed once, with qty > 0",
                d.qty, d.symbol
            ));
        }
    }
    Ok(list)
}

/// What the backstop must close. A drill symbol is spared only while its position is exactly the
/// drill's qty and no engine order ("bmv-" ids) is on it; then only that position and the owner's
/// sell orders for at most that qty survive. Anything else, on any symbol, is closed or cancelled.
fn outside_drills<'a>(
    keep: &[Drill],
    positions: &'a [Position],
    orders: &'a [Order],
) -> (Vec<&'a Position>, Vec<&'a Order>) {
    let spared = |s: &str| {
        keep.iter().find(|d| {
            d.symbol == s
                && positions.iter().any(|p| p.symbol == s && p.qty == d.qty)
                && !orders
                    .iter()
                    .any(|o| o.symbol == s && o.client_order_id.starts_with("bmv-"))
        })
    };
    let p = positions
        .iter()
        .filter(|p| spared(&p.symbol).is_none())
        .collect();
    let o = orders
        .iter()
        .filter(|o| spared(&o.symbol).is_none_or(|d| o.side != "sell" || i64::from(o.qty) > d.qty))
        .collect();
    (p, o)
}

/// For logs: what is left outside the drills.
fn describe(p: &[&Position], o: &[&Order]) -> String {
    format!(
        "positions {:?}, orders {:?}",
        p.iter()
            .map(|p| format!("{} {}", p.qty, p.symbol))
            .collect::<Vec<_>>(),
        o.iter()
            .map(|o| o.client_order_id.as_str())
            .collect::<Vec<_>>()
    )
}

/// Every symbol the run would watch must be on the allow-list.
pub fn check_watchlist(watch: &[String], allowed: &BTreeSet<String>) -> Result<(), String> {
    let off: Vec<&str> = watch
        .iter()
        .filter(|w| !allowed.contains(&w.to_ascii_uppercase()))
        .map(String::as_str)
        .collect();
    if off.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "not on the ticker allow-list (config/allowlist.txt): {}",
            off.join(", ")
        ))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Peak {
    cents: i64,
    at: String,
}

/// The drawdown auto-halt (owner, 2026-11-04): the account's high-water mark lives in
/// data/equity_peak.json and moves up at each session start. Equity more than the mode's limit
/// below it engages the kill switch, so nothing trades until the owner clears it.
pub fn check_drawdown(home: &Path, equity: Usd, limits: &Limits) -> Result<(), String> {
    let path = home.join("data").join("equity_peak.json");
    let peak = match std::fs::read_to_string(&path) {
        Ok(t) => {
            serde_json::from_str::<Peak>(&t)
                .map_err(|e| {
                    format!(
                        "{}: {e} (fix or delete it to restart the peak)",
                        path.display()
                    )
                })?
                .cents
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if equity.cents() > peak {
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string(&Peak {
            cents: equity.cents(),
            at: chrono::Utc::now().to_rfc3339(),
        })
        .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(home.join("data"))
            .and_then(|_| std::fs::write(&tmp, body))
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(());
    }
    // integer cents: equity × 100 below peak × (100 − limit)
    let floor = i128::from(peak) * i128::from(100 - limits.max_drawdown_pct());
    if i128::from(equity.cents()) * 100 < floor {
        let fell = Usd::from_cents(peak - equity.cents());
        let reason = format!(
            "drawdown halt: equity {equity} is {fell} below its peak {} (limit {}%)",
            Usd::from_cents(peak),
            limits.max_drawdown_pct()
        );
        safety::engage(&safety::stop_path(home), &reason)?;
        return Err(reason);
    }
    Ok(())
}

/// A restart can't forget today's daily-loss halt.
pub fn check_day_pnl(a: &Account, limits: &Limits) -> Result<(), String> {
    let pnl = a.equity - a.last_equity;
    if pnl <= Usd::ZERO - limits.daily_loss_halt() {
        return Err(format!("today's P&L {pnl} already hit the daily loss halt"));
    }
    Ok(())
}

pub fn open_orders(a: &Alpaca) -> Result<Vec<Order>, String> {
    a.get(Api::Trading, "/v2/orders?status=open&nested=true")
}

#[derive(serde::Deserialize)]
struct Clock {
    timestamp: String,
}

/// Every check, in order, then a Runner ready for the first tick. `mode` is the config's mode
/// (Alpaca doesn't expose it).
pub fn startup(
    alpaca: Alpaca,
    mode: Mode,
    home: &Path,
    settings: Settings,
    watchlist: Vec<String>,
    desk: Option<&crate::desk_link::Link>,
) -> Result<Runner<Alpaca>, String> {
    refuse_live(mode)?;
    let limits = Limits::for_mode(Mode::Paper);
    safety::startup_checks(&limits, home)?;
    let allowed = allowlist(home)?;
    check_watchlist(&watchlist, &allowed)?;
    let account = alpaca.account()?;
    // the desk's go-ahead for this strategy, and its tighter limits, when the link is on
    let limits = match desk {
        Some(d) => d.approve(mode, limits, account.equity)?,
        None => limits,
    };
    check_day_pnl(&account, &limits)?;
    check_drawdown(home, account.equity, &limits)?;
    let clock: Clock = alpaca.get(Api::Trading, "/v2/clock")?;
    check_skew(&clock.timestamp, chrono::Utc::now().timestamp_millis())?;
    let today = chrono::Utc::now()
        .with_timezone(&chrono_tz::America::New_York)
        .date_naive()
        .to_string();
    let rows: Vec<CalendarRow> = alpaca.get(
        Api::Trading,
        &format!("/v2/calendar?start={today}&end={today}"),
    )?;
    let session = pick_session(&rows, &today)?;
    if ran_since(&journal_path(home), session.open)? {
        return Err(format!(
            "a run already started today ({}): a restart would forget today's trades, losses and halts. \
             Not restartable until the next session",
            journal_path(home).display()
        ));
    }
    require_flat(&alpaca.positions()?, &open_orders(&alpaca)?)?;
    let journal = Journal::open(&journal_path(home))?;
    let engine = State::new(limits, settings, crate::risk::Allowed::Only(allowed))?;
    let feed = Feed::new(watchlist, session.open, session.close)?;
    Runner::new(
        engine,
        Executor::new(alpaca, journal),
        feed,
        &session,
        account.buying_power,
    )
}

/// Held for the whole run, so a second `run` (another terminal, a stray launchd job) fails
/// instead of trading the same account twice. The OS releases it when the process exits.
#[derive(Debug)]
pub struct RunLock(std::fs::File);

impl RunLock {
    /// The locked file; holding the RunLock is what matters.
    pub fn file(&self) -> &std::fs::File {
        &self.0
    }
}

pub fn lock_run(home: &Path) -> Result<RunLock, String> {
    let path = home.join("data").join("run.lock");
    std::fs::create_dir_all(home.join("data"))
        .map_err(|e| format!("run lock {}: {e}", path.display()))?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("run lock {}: {e}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(RunLock(f)),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "another `run` is active ({} is locked)",
            path.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("run lock {}: {e}", path.display())),
    }
}

/// The backstop acts from this long before the close (the engine flattens at 5 minutes)...
pub const BACKSTOP_BEFORE_SECS: i64 = 180;
/// ...until this long after it. Not longer: after the close a market order queues for tomorrow's
/// open, and flatten would keep cancelling and resubmitting it.
pub const BACKSTOP_AFTER_SECS: i64 = 60;

/// Cheap local check, no network: could `now` be near a regular (16:00) or half-day (13:00) close?
/// Weekends never are. The calendar then decides exactly.
pub fn maybe_near_close(now: i64) -> bool {
    use chrono::{Datelike, Timelike};
    let Some(t) = chrono::DateTime::from_timestamp(now, 0) else {
        return false;
    };
    let et = t.with_timezone(&chrono_tz::America::New_York);
    if et.weekday().number_from_monday() > 5 {
        return false;
    }
    let secs = i64::from(et.num_seconds_from_midnight());
    [13 * 3600, 16 * 3600]
        .iter()
        .any(|close| (close - BACKSTOP_BEFORE_SECS..=close + BACKSTOP_AFTER_SECS).contains(&secs))
}

/// The flatten backstop (DESIGN §12), run by launchd every 2 minutes: near today's real close,
/// if anything is still open (the engine died, hung or the Mac slept), close everything but the
/// owner's drill positions in `keep`.
pub fn backstop(alpaca: &Alpaca, now: i64, keep: &[Drill]) -> Result<String, String> {
    let today = chrono::DateTime::from_timestamp(now, 0)
        .ok_or("bad clock")?
        .with_timezone(&chrono_tz::America::New_York)
        .date_naive()
        .to_string();
    let rows: Vec<CalendarRow> = alpaca.get(
        Api::Trading,
        &format!("/v2/calendar?start={today}&end={today}"),
    )?;
    let session = match pick_session(&rows, &today) {
        Ok(s) => s,
        Err(_) => return Ok(format!("{today}: no session")),
    };
    if !(session.close - BACKSTOP_BEFORE_SECS..=session.close + BACKSTOP_AFTER_SECS).contains(&now)
    {
        return Ok("not near today's close".into());
    }
    let (positions, orders) = (alpaca.positions()?, open_orders(alpaca)?);
    if positions.is_empty() && orders.is_empty() {
        return Ok("flat".into());
    }
    let (p, o) = outside_drills(keep, &positions, &orders);
    if p.is_empty() && o.is_empty() {
        return Ok("flat but for the drill positions".into());
    }
    let left = describe(&p, &o);
    flatten_except(alpaca, keep, 10, &mut || {
        std::thread::sleep(Duration::from_secs(1))
    })
    .map(|()| format!("FLATTENED by the backstop: {left}"))
    .map_err(|e| format!("BACKSTOP FLATTEN FAILED ({left}): {e}"))
}

/// Ticks every LOOP_SECS until the day is done. On an error, closes everything and returns it.
pub fn run(r: &mut Runner<Alpaca>, home: &Path) -> Result<(), String> {
    let stop = safety::stop_path(home);
    let beat = heartbeat_path(home);
    loop {
        let now = chrono::Utc::now().timestamp();
        // Only monitors read this; a failed write shows there as "engine not running".
        if let Err(e) = std::fs::write(&beat, now.to_string()) {
            eprintln!("heartbeat {beat:?}: {e}");
        }
        let step = r
            .tick(now, &safety::kill_switch(&stop))
            .and_then(|_| r.finished(now));
        match step {
            Ok(true) => return Ok(()),
            Ok(false) => std::thread::sleep(Duration::from_secs(LOOP_SECS)),
            Err(e) => {
                let mut wait = || std::thread::sleep(Duration::from_secs(1));
                return Err(match flatten(r.ex.broker(), 10, &mut wait) {
                    Ok(()) => format!("{e}; flattened, no positions or open orders left"),
                    Err(f) => format!("{e}; FLATTEN FAILED: {f}"),
                });
            }
        }
    }
}

/// close_all, then up to `tries` reads of positions() and open orders, each after a `sleep`,
/// until both are empty (a leftover order could fill or open a short later).
pub fn flatten<B: Broker>(b: &B, tries: u32, sleep: &mut dyn FnMut()) -> Result<(), String> {
    b.close_all()?;
    let mut left = "never checked".to_string();
    for _ in 0..tries {
        sleep();
        left = match (b.positions(), b.open_orders()) {
            (Ok(p), Ok(o)) if p.is_empty() && o.is_empty() => return Ok(()),
            (Ok(p), Ok(o)) => require_flat(&p, &o).unwrap_err(),
            (Err(e), _) | (_, Err(e)) => format!("unreadable: {e}"),
        };
    }
    Err(format!("not flat after {tries} checks: {left}"))
}

/// `flatten`, sparing the drill positions: cancels each order and closes each position outside them,
/// re-reading up to `tries` times, each after a `sleep`, until nothing outside is left. Orders go
/// first, since a stop holding the shares blocks the close. No drills is exactly `flatten`.
pub fn flatten_except<B: Broker>(
    b: &B,
    keep: &[Drill],
    tries: u32,
    sleep: &mut dyn FnMut(),
) -> Result<(), String> {
    if keep.is_empty() {
        return flatten(b, tries, sleep);
    }
    let mut left = String::new();
    for i in 0..=tries {
        let (positions, orders) = match (b.positions(), b.open_orders()) {
            (Ok(p), Ok(o)) => (p, o),
            (Err(e), _) | (_, Err(e)) => {
                left = format!("unreadable: {e}");
                if i < tries {
                    sleep();
                }
                continue;
            }
        };
        let (p, o) = outside_drills(keep, &positions, &orders);
        if p.is_empty() && o.is_empty() {
            return Ok(());
        }
        left = describe(&p, &o);
        if i == tries {
            break;
        }
        for x in &o {
            if let Err(e) = b.cancel_by_client_id(&x.client_order_id) {
                eprintln!("cancel {}: {e}", x.client_order_id);
            }
        }
        for x in &p {
            if let Err(e) = b.close_position(&x.symbol) {
                eprintln!("close {}: {e}", x.symbol);
            }
        }
        sleep();
    }
    Err(format!(
        "not flat outside the drills after {tries} tries: {left}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Entry, JournalKind};
    use crate::risk::{self, RiskState};
    use crate::strategy::{Candidate, OrbParams};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    const ID: &str = "bmv-2024-01-02-SPY-1";
    const IDX: &str = "bmv-2024-01-02-SPY-1-x";
    const OPEN: i64 = 1_704_205_800; // 2024-01-02 09:30 ET
    const CLOSE: i64 = OPEN + 390 * 60;

    fn p(c: i64) -> Price {
        Price::from_cents(c).unwrap()
    }

    /// A temp dir removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicU64 = AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "bmv-live-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
        fn journal(&self) -> Journal {
            Journal::open(&journal_path(&self.0)).unwrap()
        }
        fn lines(&self) -> Vec<serde_json::Value> {
            std::fs::read_to_string(journal_path(&self.0))
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
        fn kinds(&self) -> Vec<String> {
            self.lines()
                .iter()
                .map(|l| l["kind"].as_str().unwrap().to_string())
                .collect()
        }
        /// A journal whose writes fail: the file is open read-only.
        fn broken_journal(&self) -> Journal {
            let path = self.0.join("ro");
            std::fs::write(&path, b"").unwrap();
            Journal {
                file: std::fs::File::open(path).unwrap(),
            }
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ord(id: &str, side: &str, status: &str, qty: u32, filled: u32, avg: Option<i64>) -> Order {
        Order {
            id: "61e69015-8549-4bfd-b9c3-01e75843f47d".into(),
            client_order_id: id.into(),
            symbol: "SPY".into(),
            side: side.into(),
            status: status.into(),
            qty,
            filled_qty: filled,
            filled_avg_price: avg.map(p),
            legs: None,
        }
    }

    fn with_legs(mut o: Order, legs: Vec<Order>) -> Order {
        o.legs = Some(legs);
        o
    }

    /// Order isn't Clone.
    fn copy(o: &Order) -> Order {
        Order {
            id: o.id.clone(),
            client_order_id: o.client_order_id.clone(),
            symbol: o.symbol.clone(),
            side: o.side.clone(),
            status: o.status.clone(),
            qty: o.qty,
            filled_qty: o.filled_qty,
            filled_avg_price: o.filled_avg_price,
            legs: o.legs.as_ref().map(|l| l.iter().map(copy).collect()),
        }
    }

    fn pos(symbol: &str, qty: i64) -> Position {
        Position {
            symbol: symbol.into(),
            qty,
            avg_entry_price: p(10_000),
        }
    }

    /// Records calls; each method fails with the next scripted message, else succeeds.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        orders: RefCell<BTreeMap<String, Order>>,
        positions: RefCell<Vec<(String, i64)>>,
        fail: RefCell<BTreeMap<&'static str, VecDeque<String>>>,
        day: RefCell<Vec<(String, Bar)>>,
        open: RefCell<Vec<Order>>,
    }

    impl Fake {
        fn script(&self, method: &'static str, msg: &str) {
            self.fail
                .borrow_mut()
                .entry(method)
                .or_default()
                .push_back(msg.into());
        }
        fn call(&self, method: &'static str, arg: &str) -> Option<String> {
            self.calls.borrow_mut().push(format!("{method} {arg}"));
            self.fail.borrow_mut().get_mut(method)?.pop_front()
        }
        fn count(&self, prefix: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with(prefix))
                .count()
        }
        fn set_order(&self, o: Order) {
            self.orders
                .borrow_mut()
                .insert(o.client_order_id.clone(), o);
        }
        fn set_positions(&self, p: &[(&str, i64)]) {
            *self.positions.borrow_mut() = p.iter().map(|(s, q)| (s.to_string(), *q)).collect();
        }
    }

    fn submit_error(m: String) -> SubmitError {
        if let Some(r) = m.strip_prefix("rejected:") {
            SubmitError::Rejected(r.into())
        } else if let Some(r) = m.strip_prefix("invalid:") {
            SubmitError::Invalid(r.into())
        } else {
            SubmitError::Unknown(m)
        }
    }

    impl Broker for Fake {
        fn submit_bracket(&self, b: Bracket) -> Result<(Order, Exits), BracketError> {
            match self.call("submit", b.client_id()).map(submit_error) {
                None => Ok((ord(b.client_id(), "buy", "new", 1, 0, None), b.into_exits())),
                Some(SubmitError::Unknown(m)) => Err(BracketError::Unknown(m, b)),
                Some(e) => Err(BracketError::Failed(e)),
            }
        }
        fn submit_oco(&self, x: &Exits, entry: &Order) -> Result<Order, SubmitError> {
            if let Some(m) = self.call("oco", &x.oco_id()) {
                return Err(submit_error(m));
            }
            let o = ord(&x.oco_id(), "sell", "new", entry.filled_qty, 0, None);
            self.set_order(copy(&o));
            Ok(o)
        }
        fn cancel_by_client_id(&self, id: &str) -> Result<Cancel, String> {
            match self.call("cancel", id).as_deref() {
                None => Ok(Cancel::Requested),
                Some("gone") => Ok(Cancel::Gone),
                Some("notcancelable") => Ok(Cancel::NotCancelable),
                Some("notfound") => Ok(Cancel::NotFound),
                Some(e) => Err(e.into()),
            }
        }
        fn close_position(&self, symbol: &str) -> Result<(), String> {
            self.call("close", symbol).map_or(Ok(()), Err)
        }
        fn close_all(&self) -> Result<(), String> {
            self.call("close_all", "").map_or(Ok(()), Err)?;
            self.positions.borrow_mut().clear();
            Ok(())
        }
        fn positions(&self) -> Result<Vec<Position>, String> {
            if let Some(e) = self.call("positions", "") {
                return Err(e);
            }
            Ok(self
                .positions
                .borrow()
                .iter()
                .map(|(s, q)| pos(s, *q))
                .collect())
        }
        fn order_by_client_id(&self, id: &str) -> Result<Option<Order>, String> {
            if let Some(e) = self.call("get", id) {
                return Err(e);
            }
            Ok(self.orders.borrow().get(id).map(copy))
        }
        fn open_orders(&self) -> Result<Vec<Order>, String> {
            if let Some(e) = self.call("open_orders", "") {
                return Err(e);
            }
            Ok(self.open.borrow().iter().map(copy).collect())
        }
        fn bars(
            &self,
            _symbols: &[String],
            from: i64,
            to: i64,
        ) -> Result<BTreeMap<String, Vec<Bar>>, String> {
            if let Some(e) = self.call("bars", "") {
                return Err(e);
            }
            let mut out: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
            for (s, b) in self.day.borrow().iter() {
                if (from..to).contains(&b.ts) {
                    out.entry(s.clone()).or_default().push(*b);
                }
            }
            Ok(out)
        }
    }

    fn intent() -> crate::risk::OrderIntent {
        let c = Candidate::long("SPY", p(10_000), p(9_900), p(10_200), String::new()).unwrap();
        let s = RiskState {
            entries_allowed: true,
            buying_power: Usd::from_cents(1_000_000_000),
            allowed: crate::risk::Allowed::Any,
            ..RiskState::default()
        };
        risk::gate(&c, Verdict::Approve, &s, &Limits::for_mode(Mode::Paper)).unwrap()
    }

    fn lt(working: bool, qty: u32, filled: u32) -> LiveTrade {
        LiveTrade {
            id: ID.into(),
            symbol: "SPY".into(),
            working,
            qty,
            filled,
            mark: p(10_050),
        }
    }

    fn ur(key: &str) -> impl Fn(&Event) -> bool + '_ {
        move |e| matches!(e, Event::Unresolved { key: k, .. } if k == key)
    }
    fn rs(key: &str) -> Event {
        resolved(key)
    }
    fn is_unexplained(e: &Event) -> bool {
        matches!(e, Event::Unexplained { .. })
    }

    fn ex(t: &Tmp) -> Executor<Fake> {
        Executor::new(Fake::default(), t.journal())
    }

    /// An executor that placed ID's bracket.
    fn placed(t: &Tmp) -> Executor<Fake> {
        let mut e = ex(t);
        assert!(e.apply(0, vec![submit()]).is_empty());
        e
    }

    fn submit() -> Effect {
        Effect::Submit {
            id: ID.into(),
            order: intent(),
        }
    }

    fn jfx() -> Effect {
        Effect::Journal {
            kind: JournalKind::OrderSubmit,
            id: ID.into(),
            detail: "d".into(),
        }
    }

    // --- Journal

    #[test]
    fn journal_writes_one_escaped_json_line_and_appends_across_reopen() {
        let t = Tmp::new();
        t.journal().write(7, "Fill", ID, "a \"q\"\nb").unwrap();
        t.journal().write(8, "Exit", "", "x").unwrap();
        let raw = std::fs::read_to_string(journal_path(&t.0)).unwrap();
        assert_eq!(raw.lines().count(), 2);
        let l = t.lines();
        assert_eq!(l[0]["ts"], 7);
        assert_eq!(l[0]["kind"], "Fill");
        assert_eq!(l[0]["id"], ID);
        assert_eq!(l[0]["detail"], "a \"q\"\nb");
        assert_eq!(l[1]["kind"], "Exit");
        assert_eq!(l[0].as_object().unwrap().len(), 4);
    }

    #[test]
    fn journal_open_fails_on_an_unwritable_path() {
        let t = Tmp::new();
        std::fs::write(t.0.join("file"), b"").unwrap();
        assert!(Journal::open(&t.0.join("file").join("j.jsonl")).is_err());
        assert!(t.broken_journal().write(0, "k", "", "").is_err());
    }

    // --- order_events

    #[test]
    fn rule1_identity_side_or_qty_mismatch_is_unexplained() {
        let t = lt(true, 24, 0);
        for o in [
            ord("bmv-2024-01-02-SPY-2", "buy", "new", 24, 0, None),
            ord(ID, "sell", "new", 24, 0, None),
            ord(ID, "buy", "new", 25, 0, None),
            Order {
                symbol: "QQQ".into(),
                ..ord(ID, "buy", "new", 24, 0, None)
            },
        ] {
            let ev = order_events(&t, &o, None, Some(0));
            assert!(ev.len() == 1 && is_unexplained(&ev[0]), "{ev:?}");
        }
        // an open trade holds less than was ordered: not a mismatch
        let ev = order_events(
            &lt(false, 10, 10),
            &ord(ID, "buy", "canceled", 24, 10, Some(10_001)),
            None,
            Some(10),
        );
        assert!(ev.is_empty(), "{ev:?}");
    }

    #[test]
    fn rule2_an_unknown_status_is_unresolved() {
        let ev = order_events(
            &lt(true, 24, 0),
            &ord(ID, "buy", "replaced", 24, 5, Some(1)),
            None,
            None,
        );
        assert_eq!(ev, vec![unresolved(ID, "status replaced")]);
    }

    #[test]
    fn rule3_a_new_fill_needs_an_average_price() {
        let ev = order_events(
            &lt(true, 24, 0),
            &ord(ID, "buy", "partially_filled", 24, 10, Some(10_001)),
            None,
            Some(10),
        );
        assert_eq!(
            ev,
            vec![Event::EntryFilled {
                id: ID.into(),
                cum_qty: Qty::new(10).unwrap(),
                avg_price: p(10_001)
            }]
        );
        let ev = order_events(
            &lt(true, 24, 0),
            &ord(ID, "buy", "partially_filled", 24, 10, None),
            None,
            Some(10),
        );
        assert!(ev.len() == 1 && ur(ID)(&ev[0]), "{ev:?}");
        // already known: nothing
        let ev = order_events(
            &lt(true, 24, 10),
            &ord(ID, "buy", "partially_filled", 24, 10, Some(10_001)),
            None,
            Some(10),
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn rule4_a_closed_working_entry_is_entry_closed() {
        for s in ENTRY_CLOSED {
            let ev = order_events(
                &lt(true, 24, 0),
                &ord(ID, "buy", s, 24, 0, None),
                None,
                Some(0),
            );
            assert_eq!(ev, vec![Event::EntryClosed { id: ID.into() }], "{s}");
        }
        let ev = order_events(
            &lt(true, 24, 10),
            &ord(ID, "buy", "canceled", 24, 10, Some(10_001)),
            None,
            Some(10),
        );
        assert_eq!(ev, vec![Event::EntryClosed { id: ID.into() }]);
    }

    #[test]
    fn rule5_sell_legs_covering_the_fill_exit_at_the_floor_of_their_weighted_price() {
        let entry = with_legs(
            ord(ID, "buy", "filled", 10, 10, Some(10_000)),
            vec![
                ord("l1", "sell", "filled", 10, 4, Some(10_200)),
                ord("l2", "sell", "filled", 10, 6, Some(9_901)),
            ],
        );
        let ev = order_events(&lt(false, 10, 10), &entry, None, Some(10));
        // (4·10200 + 6·9901) / 10 = 10020.6
        assert_eq!(
            ev,
            vec![Event::Exited {
                id: ID.into(),
                price: p(10_020)
            }]
        );
        // only part sold: still held
        let part = with_legs(
            ord(ID, "buy", "filled", 10, 10, Some(10_000)),
            vec![ord("l1", "sell", "partially_filled", 10, 4, Some(10_200))],
        );
        assert!(order_events(&lt(false, 10, 10), &part, None, Some(6)).is_empty());
    }

    #[test]
    fn rule5_an_oco_exit_counts() {
        let entry = with_legs(
            ord(ID, "buy", "canceled", 24, 10, Some(10_000)),
            vec![ord("l1", "sell", "held", 24, 0, None)],
        );
        let oco = with_legs(
            ord(IDX, "sell", "canceled", 10, 0, None),
            vec![ord("s", "sell", "filled", 10, 10, Some(9_900))],
        );
        let ev = order_events(&lt(false, 10, 10), &entry, Some(&oco), Some(10));
        assert_eq!(
            ev,
            vec![Event::Exited {
                id: ID.into(),
                price: p(9_900)
            }]
        );
    }

    #[test]
    fn rule6_a_vanished_position_exits_at_the_mark() {
        let entry = ord(ID, "buy", "filled", 10, 10, Some(10_000));
        let ev = order_events(&lt(false, 10, 10), &entry, None, Some(0));
        assert_eq!(
            ev,
            vec![Event::Exited {
                id: ID.into(),
                price: p(10_050)
            }]
        );
        assert!(order_events(&lt(false, 10, 10), &entry, None, None).is_empty());
        assert!(order_events(&lt(false, 10, 10), &entry, None, Some(10)).is_empty());
        // nothing held yet: no exit
        assert!(
            order_events(
                &lt(true, 10, 0),
                &ord(ID, "buy", "new", 10, 0, None),
                None,
                Some(0)
            )
            .is_empty()
        );
    }

    #[test]
    fn position_events_flag_what_no_trade_explains() {
        let trades = [lt(false, 10, 10)];
        assert!(position_events(&trades, &[pos("SPY", 10)]).is_empty());
        for bad in [pos("SPY", -1), pos("SPY", 11), pos("AMD", 1)] {
            let ev = position_events(&trades, &[bad]);
            assert!(ev.len() == 1 && is_unexplained(&ev[0]), "{ev:?}");
        }
    }

    // --- Effect table

    #[test]
    fn journal_effect_writes_and_a_failure_blocks_submits_until_a_write_succeeds() {
        let t = Tmp::new();
        let mut e = ex(&t);
        assert!(e.apply(5, vec![jfx()]).is_empty());
        assert_eq!(t.kinds(), ["OrderSubmit"]);

        e.journal = t.broken_journal();
        let ev = e.apply(6, vec![jfx(), submit()]);
        assert!(ur("journal")(&ev[0]), "{ev:?}");
        assert!(ev.contains(&Event::EntryClosed { id: ID.into() }));
        assert_eq!(e.broker().count("submit"), 0);

        e.journal = t.journal();
        assert_eq!(e.apply(7, vec![jfx()]), vec![rs("journal")]);
    }

    #[test]
    fn submit_ok_stores_the_exits_and_journals_the_call() {
        let t = Tmp::new();
        let e = placed(&t);
        assert!(e.exits.contains_key(ID));
        assert_eq!(t.kinds(), ["broker.submit"]);
    }

    #[test]
    fn submit_rejected_closes_and_invalid_also_blocks_the_day() {
        let t = Tmp::new();
        let mut e = ex(&t);
        e.broker().script("submit", "rejected:no buying power");
        assert_eq!(
            e.apply(0, vec![submit()]),
            vec![Event::EntryClosed { id: ID.into() }]
        );
        e.broker().script("submit", "invalid:bad id");
        let ev = e.apply(0, vec![submit()]);
        assert_eq!(ev[0], Event::EntryClosed { id: ID.into() });
        assert!(ur(&format!("bug:{ID}"))(&ev[1]), "{ev:?}");
        assert!(e.exits.is_empty());
    }

    #[test]
    fn an_unknown_submit_is_never_resent_and_a_later_poll_resolves_it() {
        let t = Tmp::new();
        let mut e = ex(&t);
        e.broker().script("submit", "timeout");
        let ev = e.apply(0, vec![submit()]);
        assert!(ev.len() == 1 && ur(ID)(&ev[0]), "{ev:?}");
        assert!(e.unknown.contains_key(ID));

        // not found yet: still unresolved, nothing resent
        let ev = e.reconcile(1, &[lt(true, 24, 0)]);
        assert!(ev.iter().any(ur(ID)), "{ev:?}");
        e.retry(2);
        assert_eq!(e.broker().count("submit"), 1);

        let q = intent().qty().get();
        e.broker().set_order(ord(ID, "buy", "new", q, 0, None));
        let ev = e.reconcile(3, &[lt(true, q, 0)]);
        assert_eq!(ev, vec![rs("positions"), rs(ID)]);
        assert!(e.unknown.is_empty() && e.exits.contains_key(ID));
        assert_eq!(e.broker().count("submit"), 1);
    }

    #[test]
    fn cancel_outcomes() {
        let t = Tmp::new();
        let mut e = ex(&t);
        let cancel = || Effect::CancelEntry { id: ID.into() };
        assert!(e.apply(0, vec![cancel()]).is_empty());
        for s in ["gone", "notcancelable"] {
            e.broker().script("cancel", s);
            assert!(e.apply(0, vec![cancel()]).is_empty(), "{s}");
        }
        e.broker().script("cancel", "notfound");
        let ev = e.apply(0, vec![cancel()]);
        assert!(ev.len() == 1 && ur(ID)(&ev[0]));

        let key = format!("cancel:{ID}");
        e.broker().script("cancel", "HTTP 500");
        let ev = e.apply(0, vec![cancel()]);
        assert!(ev.len() == 1 && ur(&key)(&ev[0]));
        e.broker().script("cancel", "HTTP 500");
        let ev = e.retry(1);
        assert!(ev.len() == 1 && ur(&key)(&ev[0]));
        assert_eq!(e.retry(2), vec![rs(&key)]);
        assert!(e.retry(3).is_empty());
        assert_eq!(
            t.kinds().iter().filter(|k| *k == "broker.cancel").count(),
            7
        );
    }

    fn partial(e: &Executor<Fake>) {
        e.broker().set_order(ord(
            ID,
            "buy",
            "canceled",
            intent().qty().get(),
            10,
            Some(10_000),
        ));
    }

    #[test]
    fn protect_places_an_oco_for_the_filled_shares() {
        let t = Tmp::new();
        let mut e = placed(&t);
        partial(&e);
        let ev = e.apply(0, vec![Effect::Protect { id: ID.into() }]);
        assert_eq!(ev, vec![rs(IDX)]);
        assert!(e.oco.contains(ID));
        assert_eq!(e.broker().orders.borrow()[IDX].qty, 10);
    }

    #[test]
    fn protect_without_a_placed_bracket_is_unexplained() {
        let t = Tmp::new();
        let mut e = ex(&t);
        let ev = e.apply(0, vec![Effect::Protect { id: ID.into() }]);
        assert!(ev.len() == 1 && is_unexplained(&ev[0]));
    }

    #[test]
    fn protect_lookup_failure_retries() {
        let t = Tmp::new();
        let mut e = placed(&t);
        let ev = e.apply(0, vec![Effect::Protect { id: ID.into() }]); // entry not found
        assert!(ev.len() == 1 && ur(IDX)(&ev[0]), "{ev:?}");
        e.broker().script("get", "HTTP 500"); // the "-x" lookup
        let ev = e.retry(1);
        assert!(ev.len() == 1 && ur(IDX)(&ev[0]), "{ev:?}");
        partial(&e);
        assert_eq!(e.retry(2), vec![rs(IDX)]);
        assert!(e.retry_protect.is_empty());
    }

    #[test]
    fn protect_rejected_closes_the_position() {
        let t = Tmp::new();
        let mut e = placed(&t);
        partial(&e);
        e.broker().script("oco", "rejected:through the stop");
        let ev = e.apply(0, vec![Effect::Protect { id: ID.into() }]);
        assert!(ev.len() == 1 && ur(IDX)(&ev[0]), "{ev:?}");
        assert_eq!(e.broker().count("close SPY"), 1);
        assert!(e.retry_protect.is_empty());
        e.closing.clear();

        e.broker().script("oco", "rejected:through the stop");
        e.broker().script("close", "HTTP 500");
        let ev = e.apply(0, vec![Effect::Protect { id: ID.into() }]);
        assert!(ev.len() == 1 && is_unexplained(&ev[0]), "{ev:?}");
        assert!(t.kinds().contains(&"broker.close".to_string()));
    }

    #[test]
    fn an_accepted_close_stays_unresolved_until_the_position_is_gone() {
        let t = Tmp::new();
        let tr = [lt(false, 10, 10)];
        for (gone_at, expect_resolved) in [(10, true), (CLOSE_CONFIRM_SECS, false)] {
            let mut e = placed(&t);
            partial(&e);
            e.broker().script("oco", "rejected:through the stop");
            e.apply(0, vec![Effect::Protect { id: ID.into() }]);
            e.broker().set_positions(&[("SPY", 10)]);
            let ev = e.reconcile(5, &tr);
            assert!(
                !ev.contains(&rs(IDX)) && !ev.iter().any(is_unexplained),
                "{ev:?}"
            );
            if expect_resolved {
                e.broker().set_positions(&[]);
            }
            let ev = e.reconcile(gone_at, &tr);
            assert_eq!(ev.contains(&rs(IDX)), expect_resolved, "{ev:?}");
            assert_eq!(ev.iter().any(is_unexplained), !expect_resolved, "{ev:?}");
            assert!(e.closing.is_empty());
        }
    }

    #[test]
    fn protect_invalid_is_unexplained() {
        let t = Tmp::new();
        let mut e = placed(&t);
        partial(&e);
        e.broker().script("oco", "invalid:over approved");
        let ev = e.apply(0, vec![Effect::Protect { id: ID.into() }]);
        assert!(ev.len() == 1 && is_unexplained(&ev[0]));
    }

    #[test]
    fn protect_unknown_retry_finds_the_oco_and_does_not_resend() {
        let t = Tmp::new();
        let mut e = placed(&t);
        partial(&e);
        e.broker().script("oco", "timeout");
        let ev = e.apply(0, vec![Effect::Protect { id: ID.into() }]);
        assert!(ev.len() == 1 && ur(IDX)(&ev[0]));
        e.broker().set_order(ord(IDX, "sell", "new", 10, 0, None)); // it had landed
        assert_eq!(e.retry(1), vec![rs(IDX)]);
        assert_eq!(e.broker().count("oco"), 1);
        assert!(e.oco.contains(ID) && e.retry_protect.is_empty());
    }

    #[test]
    fn protect_is_skipped_after_a_flatten() {
        let t = Tmp::new();
        let mut e = placed(&t);
        partial(&e);
        assert_eq!(e.apply(0, vec![Effect::FlattenAll]), vec![rs("flatten")]);
        assert!(
            e.apply(0, vec![Effect::Protect { id: ID.into() }])
                .is_empty()
        );
        assert_eq!(e.broker().count("oco") + e.broker().count("get"), 0);
        assert!(t.kinds().contains(&"broker.oco".to_string()));
    }

    #[test]
    fn a_failed_close_all_is_retried() {
        let t = Tmp::new();
        let mut e = ex(&t);
        e.broker().script("close_all", "HTTP 500");
        let ev = e.apply(0, vec![Effect::FlattenAll]);
        assert!(ev.len() == 1 && ur("flatten")(&ev[0]));
        assert_eq!(e.retry(1), vec![rs("flatten")]);
        assert!(e.retry(2).is_empty());
        assert_eq!(e.broker().count("close_all"), 2);
    }

    #[test]
    fn vet_is_rejected_until_p4() {
        let t = Tmp::new();
        let c = Candidate::long("SPY", p(10_000), p(9_900), p(10_200), String::new()).unwrap();
        let ev = ex(&t).apply(
            0,
            vec![Effect::Vet {
                id: ID.into(),
                candidate: c,
            }],
        );
        assert_eq!(
            ev,
            vec![Event::Vetted {
                id: ID.into(),
                verdict: Verdict::Reject
            }]
        );
    }

    // --- Poll table

    #[test]
    fn positions_poll() {
        let t = Tmp::new();
        let mut e = ex(&t);
        e.broker().set_positions(&[("AMD", 5)]);
        let ev = e.reconcile(0, &[]);
        assert_eq!(ev[0], rs("positions"));
        assert!(is_unexplained(&ev[1]));
        assert!(t.kinds().contains(&"broker.poll".to_string()));

        // failed: unresolved, and a held trade isn't taken as exited
        e.broker()
            .set_order(ord(ID, "buy", "filled", 10, 10, Some(10_000)));
        e.broker().script("positions", "HTTP 500");
        let ev = e.reconcile(1, &[lt(false, 10, 10)]);
        assert!(ur("positions")(&ev[0]), "{ev:?}");
        assert_eq!(ev[1..], [rs(ID)]);
    }

    #[test]
    fn a_flat_position_needs_two_polls_and_a_lagging_one_is_explained_briefly() {
        let t = Tmp::new();
        let mut e = ex(&t);
        e.broker()
            .set_order(ord(ID, "buy", "filled", 10, 10, Some(10_000)));
        let tr = [lt(false, 10, 10)];
        let exited = |ev: &[Event]| ev.iter().any(|e| matches!(e, Event::Exited { .. }));
        assert!(!exited(&e.reconcile(0, &tr))); // positions() lags the fill
        e.broker().set_positions(&[("SPY", 10)]);
        assert!(!exited(&e.reconcile(5, &tr))); // it shows up: the count restarts
        e.broker().set_positions(&[]);
        assert!(!exited(&e.reconcile(10, &tr)));
        assert!(exited(&e.reconcile(15, &tr)));

        // the trade is gone, but a lagging read still shows its shares
        e.broker().set_positions(&[("SPY", 10)]);
        assert!(
            !e.reconcile(15 + EXIT_GRACE_SECS, &[])
                .iter()
                .any(is_unexplained)
        );
        assert!(
            e.reconcile(16 + EXIT_GRACE_SECS, &[])
                .iter()
                .any(is_unexplained)
        );
    }

    #[test]
    fn order_poll_outcomes() {
        let t = Tmp::new();
        let mut e = ex(&t);
        let tr = [lt(true, 10, 0)];
        let ev = e.reconcile(0, &tr);
        assert!(ur(ID)(&ev[1]), "{ev:?}");
        e.broker().script("get", "HTTP 500");
        let ev = e.reconcile(0, &tr);
        assert!(ur(ID)(&ev[1]), "{ev:?}");

        e.broker()
            .set_order(ord(ID, "buy", "partially_filled", 10, 4, Some(10_000)));
        e.broker().set_positions(&[("SPY", 4)]);
        let ev = e.reconcile(0, &tr);
        assert_eq!(ev[..2], [rs("positions"), rs(ID)]);
        assert!(matches!(ev[2], Event::EntryFilled { .. }));

        // an unknown status: no Resolved for the trade
        e.broker()
            .set_order(ord(ID, "buy", "stopped", 10, 4, Some(10_000)));
        let ev = e.reconcile(0, &tr);
        assert!(!ev.contains(&rs(ID)) && ev.iter().any(ur(ID)), "{ev:?}");
    }

    #[test]
    fn oco_poll_feeds_the_exit_or_is_unresolved() {
        let t = Tmp::new();
        let mut e = placed(&t);
        partial(&e);
        e.apply(0, vec![Effect::Protect { id: ID.into() }]);
        e.broker().set_positions(&[("SPY", 10)]);
        let tr = [lt(false, 10, 10)];
        e.broker().orders.borrow_mut().remove(IDX);
        let ev = e.reconcile(1, &tr);
        assert!(ev.iter().any(ur(IDX)), "{ev:?}");

        e.broker()
            .set_order(ord(IDX, "sell", "filled", 10, 10, Some(10_200)));
        e.broker().set_positions(&[]);
        let ev = e.reconcile(2, &tr);
        assert!(ev.contains(&rs(IDX)));
        assert_eq!(
            ev.last(),
            Some(&Event::Exited {
                id: ID.into(),
                price: p(10_200)
            })
        );
    }

    // --- drive and the runner

    fn engine() -> State {
        State::new(
            Limits::for_mode(Mode::Paper),
            Settings {
                vetting: false,
                orb: OrbParams::default(),
                entry: Entry::Orb,
                trend_filter: false,
            },
            crate::risk::Allowed::Any,
        )
        .unwrap()
    }

    #[test]
    fn the_allow_list_fails_closed_and_the_drawdown_halt_latches() {
        let t = Tmp::new();
        let home = &t.0;
        assert!(allowlist(home).is_err(), "no file, no trading");
        std::fs::create_dir_all(home.join("config")).unwrap();
        let list = home.join("config/allowlist.txt");
        std::fs::write(&list, "# only comments\n\n").unwrap();
        assert!(allowlist(home).is_err(), "empty, no trading");
        std::fs::write(&list, "SPY\nqqq  # index\nBRK.B\n").unwrap();
        let names = allowlist(home).unwrap();
        assert_eq!(names.len(), 3);
        assert!(check_watchlist(&["spy".into(), "QQQ".into()], &names).is_ok());
        let off = check_watchlist(&["SPY".into(), "TSLA".into()], &names).unwrap_err();
        assert!(off.contains("TSLA") && !off.contains("SPY,"));
        std::fs::write(&list, "SPY\nnot a ticker\n").unwrap();
        assert!(allowlist(home).is_err());

        let paper = Limits::for_mode(Mode::Paper);
        let usd = |d: i64| Usd::from_cents(d * 100);
        check_drawdown(home, usd(25_000), &paper).unwrap();
        check_drawdown(home, usd(26_000), &paper).unwrap(); // a new peak
        check_drawdown(home, usd(23_500), &paper).unwrap(); // 9.6% down: inside 10%
        assert_eq!(
            safety::kill_switch(&safety::stop_path(home)),
            KillSwitch::Clear
        );
        let e = check_drawdown(home, usd(23_300), &paper).unwrap_err(); // 10.4% down
        assert!(e.contains("drawdown halt"), "{e}");
        assert!(matches!(
            safety::kill_switch(&safety::stop_path(home)),
            KillSwitch::Engaged(_)
        ));
        // live is tighter: 5%
        let live = Limits::for_mode(Mode::Live);
        let t2 = Tmp::new();
        check_drawdown(&t2.0, usd(1_000), &live).unwrap();
        assert!(check_drawdown(&t2.0, usd(940), &live).is_err());
    }

    #[test]
    fn drive_stops_a_feedback_loop() {
        let t = Tmp::new();
        let mut e = ex(&t);
        let r = drive(
            &mut engine(),
            &mut e,
            0,
            vec![Event::Tick(0); MAX_EVENTS_PER_DRIVE + 1],
        );
        assert!(r.is_err());
        assert_eq!(
            drive(&mut engine(), &mut e, 0, vec![Event::Tick(0)])
                .unwrap()
                .len(),
            1
        );
    }

    fn bar(minute: i64, high: f64, low: f64, close: f64, volume: u64) -> (String, Bar) {
        let b = Bar {
            ts: OPEN + minute * 60,
            open: (high + low) / 2.0,
            high,
            low,
            close,
            volume,
        };
        ("SPY".into(), b)
    }

    /// A 15-minute range 100–101, then a breakout at minute 15 (as in the engine tests).
    fn runner(t: &Tmp) -> Runner<Fake> {
        let fake = Fake::default();
        let mut day: Vec<(String, Bar)> =
            (0..15).map(|m| bar(m, 101.0, 100.0, 100.5, 1000)).collect();
        day.push(bar(15, 101.6, 100.9, 101.5, 3000));
        *fake.day.borrow_mut() = day;
        let session = Session {
            date: "2024-01-02".into(),
            open: OPEN,
            close: CLOSE,
        };
        Runner::new(
            engine(),
            Executor::new(fake, t.journal()),
            Feed::new(vec!["SPY".into()], OPEN, CLOSE).unwrap(),
            &session,
            Usd::from_cents(10_000_000),
        )
        .unwrap()
    }

    /// Ticks once a minute from the open up to the breakout's submit.
    fn to_submit(r: &mut Runner<Fake>) {
        for k in 0..=16 {
            r.tick(OPEN + k * 60 + 5, &KillSwitch::Clear).unwrap();
        }
        assert_eq!(r.ex.broker().count(&format!("submit {ID}")), 1);
    }

    #[test]
    fn a_fake_paper_day_submit_fill_target_exit() {
        let t = Tmp::new();
        let mut r = runner(&t);
        to_submit(&mut r);
        let q = r.engine.live_trades()[0].qty;
        let leg = |status: &str, filled, avg| ord("leg", "sell", status, q, filled, avg);
        r.ex.broker().set_order(with_legs(
            ord(ID, "buy", "filled", q, q, Some(10_150)),
            vec![leg("held", 0, None)],
        ));
        r.ex.broker().set_positions(&[("SPY", i64::from(q))]);
        r.tick(OPEN + 970, &KillSwitch::Clear).unwrap();
        assert_eq!(r.engine.live_trades()[0].filled, q);

        r.ex.broker().set_order(with_legs(
            ord(ID, "buy", "filled", q, q, Some(10_150)),
            vec![leg("filled", q, Some(10_300))],
        ));
        r.ex.broker().set_positions(&[]);
        let fed = r.tick(OPEN + 975, &KillSwitch::Clear).unwrap();
        assert!(
            fed.contains(&Event::Exited {
                id: ID.into(),
                price: p(10_300)
            }),
            "{fed:?}"
        );
        assert!(r.engine.live_trades().is_empty());
        assert!(r.engine.pnl_today() > Usd::ZERO && !r.engine.halted());
        let kinds = t.kinds();
        for k in [
            "SessionStart",
            "OrderSubmit",
            "broker.submit",
            "Fill",
            "Exit",
        ] {
            assert!(kinds.iter().any(|x| x == k), "{k} missing from {kinds:?}");
        }
    }

    #[test]
    fn a_mid_session_catch_up_cannot_submit_and_bars_come_before_resolved() {
        let t = Tmp::new();
        let mut r = runner(&t);
        let fed = r.tick(OPEN + 16 * 60 + 5, &KillSwitch::Clear).unwrap();
        let last_bar = fed
            .iter()
            .rposition(|e| matches!(e, Event::Bar { .. }))
            .unwrap();
        let resolved_at = fed.iter().position(|e| *e == rs(MARKET_DATA)).unwrap();
        assert!(last_bar < resolved_at);
        r.tick(OPEN + 17 * 60 + 5, &KillSwitch::Clear).unwrap();
        assert_eq!(r.ex.broker().count("submit"), 0);
    }

    #[test]
    fn a_failed_bar_fetch_stays_unresolved_and_is_retried() {
        let t = Tmp::new();
        let mut r = runner(&t);
        r.tick(OPEN + 5, &KillSwitch::Clear).unwrap();
        r.ex.broker().script("bars", "HTTP 500");
        let fed = r.tick(OPEN + 65, &KillSwitch::Clear).unwrap();
        assert!(fed.iter().any(ur(MARKET_DATA)) && !fed.contains(&rs(MARKET_DATA)));
        let fed = r.tick(OPEN + 70, &KillSwitch::Clear).unwrap();
        assert!(
            fed.iter().any(|e| matches!(e, Event::Bar { .. })) && fed.contains(&rs(MARKET_DATA))
        );
    }

    #[test]
    fn the_kill_switch_sends_an_event_only_on_change() {
        let t = Tmp::new();
        let mut r = runner(&t);
        let kills = |fed: &[Event]| {
            fed.iter()
                .filter(|e| matches!(e, Event::KillSwitch(_)))
                .cloned()
                .collect::<Vec<_>>()
        };
        let on = KillSwitch::Engaged("STOP".into());
        assert!(kills(&r.tick(OPEN + 5, &KillSwitch::Clear).unwrap()).is_empty());
        assert_eq!(
            kills(&r.tick(OPEN + 10, &on).unwrap()),
            [Event::KillSwitch(true)]
        );
        assert!(kills(&r.tick(OPEN + 15, &on).unwrap()).is_empty());
        assert_eq!(
            kills(&r.tick(OPEN + 20, &KillSwitch::Clear).unwrap()),
            [Event::KillSwitch(false)]
        );
    }

    #[test]
    fn finished_after_the_close_when_flat_and_an_error_if_still_live() {
        let t = Tmp::new();
        let r = runner(&t);
        assert_eq!(r.finished(CLOSE + 299), Ok(false));
        assert_eq!(r.finished(CLOSE + 300), Ok(true));
        let t2 = Tmp::new();
        let mut r = runner(&t2);
        to_submit(&mut r);
        assert_eq!(r.finished(CLOSE + 300), Ok(false));
        assert!(r.finished(CLOSE + 900).is_err());
    }

    // --- startup and flatten

    #[test]
    fn run_refuses_live() {
        assert!(refuse_live(Mode::Paper).is_ok());
        assert!(refuse_live(Mode::Live).unwrap_err().contains("P5"));
    }

    #[test]
    fn clock_skew_over_two_seconds_or_unreadable_refuses() {
        let server = "2024-01-02T14:30:00Z";
        let ms = OPEN * 1000;
        for d in [-2_000, 0, 2_000] {
            assert!(check_skew(server, ms + d).is_ok(), "{d}");
        }
        for d in [-2_001, 2_001] {
            assert!(check_skew(server, ms + d).is_err(), "{d}");
        }
        assert!(check_skew("2024-01-02T09:30:00.5-05:00", ms + 500).is_ok());
        assert!(check_skew("garbage", ms).is_err());
    }

    #[test]
    fn a_second_run_lock_is_refused_until_the_first_is_dropped() {
        let t = Tmp::new();
        let first = lock_run(&t.0).unwrap(); // creates data/
        assert!(first.file().metadata().is_ok());
        let err = lock_run(&t.0).unwrap_err();
        assert!(err.contains("another `run` is active"), "{err}");
        drop(first);
        assert!(lock_run(&t.0).is_ok());
    }

    #[test]
    fn the_backstop_only_wakes_near_a_close_on_weekdays() {
        // 2024-01-02 (Tue) 09:30 ET = OPEN; 16:00 ET = OPEN + 6.5 h
        let close = OPEN + 390 * 60;
        assert!(!maybe_near_close(OPEN));
        assert!(!maybe_near_close(close - BACKSTOP_BEFORE_SECS - 1));
        assert!(maybe_near_close(close - BACKSTOP_BEFORE_SECS));
        assert!(maybe_near_close(close + BACKSTOP_AFTER_SECS));
        assert!(!maybe_near_close(close + BACKSTOP_AFTER_SECS + 1));
        assert!(maybe_near_close(close - 3 * 3600), "13:00 half-day close");
        assert!(!maybe_near_close(close + 4 * 86_400), "Saturday");
    }

    #[test]
    fn pick_session_needs_exactly_todays_row() {
        let row = |d: &str| CalendarRow {
            date: d.into(),
            open: "09:30".into(),
            close: "16:00".into(),
        };
        let s = pick_session(&[row("2024-01-02")], "2024-01-02").unwrap();
        assert_eq!((s.open, s.close), (OPEN, CLOSE));
        assert!(pick_session(&[], "2024-01-02").is_err());
        assert!(pick_session(&[row("2024-01-03")], "2024-01-02").is_err());
        assert!(pick_session(&[row("2024-01-02"), row("2024-01-02")], "2024-01-02").is_err());
    }

    #[test]
    fn startup_requires_a_flat_account() {
        assert!(require_flat(&[], &[]).is_ok());
        assert!(
            require_flat(&[pos("SPY", 1)], &[])
                .unwrap_err()
                .contains("flatten")
        );
        assert!(require_flat(&[], &[ord(ID, "buy", "new", 1, 0, None)]).is_err());
    }

    #[test]
    fn startup_refuses_after_the_daily_loss_halt() {
        let acct = |equity: i64| Account {
            status: "ACTIVE".into(),
            currency: "USD".into(),
            cash: Usd::from_cents(equity),
            buying_power: Usd::from_cents(equity),
            equity: Usd::from_cents(equity),
            last_equity: Usd::from_cents(2_500_000),
            trading_blocked: false,
            account_blocked: false,
        };
        let l = Limits::for_mode(Mode::Paper);
        let halt = l.daily_loss_halt().cents();
        assert!(check_day_pnl(&acct(2_500_000 - halt + 1), &l).is_ok());
        assert!(check_day_pnl(&acct(2_500_000 - halt), &l).is_err());
    }

    #[test]
    fn flatten_confirms_with_positions() {
        let f = Fake::default();
        let mut sleeps = 0;
        assert!(flatten(&f, 3, &mut || sleeps += 1).is_ok());
        assert_eq!(sleeps, 1);

        // unconfirmed for two polls
        f.script("positions", "HTTP 500");
        f.script("positions", "HTTP 500");
        let mut sleeps = 0;
        assert!(flatten(&f, 3, &mut || sleeps += 1).is_ok());
        assert_eq!(sleeps, 3);

        for _ in 0..3 {
            f.script("positions", "HTTP 500");
        }
        assert!(flatten(&f, 3, &mut || {}).unwrap_err().contains("not flat"));

        f.script("close_all", "HTTP 500");
        assert!(flatten(&f, 3, &mut || {}).is_err());
        assert_eq!(f.count("positions"), 7);

        // flat positions but an order left open is not flat
        f.open.borrow_mut().push(ord(ID, "buy", "new", 1, 0, None));
        let err = flatten(&f, 2, &mut || {}).unwrap_err();
        assert!(err.contains(ID), "{err}");
        f.script("open_orders", "HTTP 500");
        f.open.borrow_mut().clear();
        assert!(flatten(&f, 2, &mut || {}).is_ok());
    }

    fn drill(symbol: &str, qty: i64) -> Drill {
        Drill {
            symbol: symbol.into(),
            qty,
        }
    }

    fn on(symbol: &str, mut o: Order) -> Order {
        o.symbol = symbol.into();
        o
    }

    #[test]
    fn the_drill_file_is_missing_valid_or_an_error() {
        let t = Tmp::new();
        assert_eq!(drills(&t.0).unwrap(), vec![]);
        std::fs::create_dir_all(t.0.join("data")).unwrap();
        let write = |s: &str| std::fs::write(t.0.join("data").join("drill.json"), s).unwrap();
        write(r#"[{"symbol":"SPY","qty":1}]"#);
        assert_eq!(drills(&t.0).unwrap(), vec![drill("SPY", 1)]);
        for bad in [
            "",
            "{",
            r#"[{"symbol":"SPY"}]"#,
            r#"[{"symbol":"SPY","qty":1,"why":"x"}]"#,
            r#"[{"symbol":"SPY","qty":0}]"#,
            r#"[{"symbol":"SPY","qty":-1}]"#,
            r#"[{"symbol":"spy","qty":1}]"#,
            r#"[{"symbol":"../x","qty":1}]"#,
            r#"[{"symbol":"SPY","qty":1},{"symbol":"SPY","qty":2}]"#,
        ] {
            write(bad);
            assert!(drills(&t.0).is_err(), "{bad:?} was accepted");
        }
    }

    fn names(p: &[&Position], o: &[&Order]) -> (Vec<String>, Vec<String>) {
        (
            p.iter()
                .map(|p| format!("{} {}", p.qty, p.symbol))
                .collect(),
            o.iter().map(|o| o.client_order_id.clone()).collect(),
        )
    }

    #[test]
    fn only_an_exact_drill_and_its_owner_stop_are_spared() {
        let keep = [drill("SPY", 1)];
        let stop = || on("SPY", ord("owner-stop", "sell", "new", 1, 0, None));
        let positions = [pos("SPY", 1), pos("AAPL", 5)];
        let orders = [
            stop(),
            on("SPY", ord("owner-buy", "buy", "new", 1, 0, None)),
            on("SPY", ord("owner-big", "sell", "new", 2, 0, None)),
            on("AAPL", ord("owner-aapl", "sell", "new", 5, 0, None)),
        ];
        let (p, o) = outside_drills(&keep, &positions, &orders);
        assert_eq!(
            names(&p, &o),
            (
                vec!["5 AAPL".into()],
                vec!["owner-buy".into(), "owner-big".into(), "owner-aapl".into()]
            )
        );

        // not the drill's size, or no position at all: the symbol isn't spared, so the owner's
        // stop is cancelled too (it would hold the shares and block the close, or open a short)
        for held in [vec![pos("SPY", 3)], vec![pos("SPY", -1)], vec![]] {
            let stops = [stop()];
            let (p, o) = outside_drills(&keep, &held, &stops);
            assert_eq!((p.len(), o.len()), (held.len(), 1), "{held:?}");
        }

        // an engine order on the symbol means the position may be the engine's: nothing spared
        let engine = [stop(), on("SPY", ord(IDX, "sell", "new", 1, 0, None))];
        let one = [pos("SPY", 1)];
        let (p, o) = outside_drills(&keep, &one, &engine);
        assert_eq!(
            names(&p, &o),
            (vec!["1 SPY".into()], vec!["owner-stop".into(), IDX.into()])
        );
    }

    #[test]
    fn flatten_except_cancels_and_closes_only_outside_the_drill() {
        let keep = [drill("SPY", 1)];
        let f = Fake::default();
        f.set_positions(&[("SPY", 1), ("AAPL", 5)]);
        f.open
            .borrow_mut()
            .push(on("SPY", ord("owner-stop", "sell", "new", 1, 0, None)));
        f.open
            .borrow_mut()
            .push(on("AAPL", ord(ID, "buy", "new", 5, 0, None)));
        // the fake never clears, so it ends "not flat" after acting on every try
        let err = flatten_except(&f, &keep, 2, &mut || {}).unwrap_err();
        assert!(
            err.contains("AAPL") && err.contains(ID) && !err.contains("owner-stop"),
            "{err}"
        );
        assert_eq!(f.count("cancel bmv-"), 2);
        assert_eq!(f.count("close AAPL"), 2);
        assert_eq!(
            f.count("close SPY") + f.count("close_all") + f.count("cancel owner"),
            0
        );

        // only the drill left: nothing is touched
        let g = Fake::default();
        g.set_positions(&[("SPY", 1)]);
        g.open
            .borrow_mut()
            .push(on("SPY", ord("owner-stop", "sell", "new", 1, 0, None)));
        assert!(flatten_except(&g, &keep, 2, &mut || {}).is_ok());
        assert_eq!(g.count("close") + g.count("cancel"), 0);

        // unreadable on every try is an error, never success
        let h = Fake::default();
        for _ in 0..3 {
            h.script("positions", "HTTP 500");
        }
        let err = flatten_except(&h, &keep, 2, &mut || {}).unwrap_err();
        assert!(err.contains("unreadable"), "{err}");

        // no drills is the plain flatten: close_all
        let k = Fake::default();
        k.set_positions(&[("SPY", 1)]);
        assert!(flatten_except(&k, &[], 2, &mut || {}).is_ok());
        assert_eq!(k.count("close_all"), 1);
    }

    #[test]
    fn a_restart_after_this_sessions_open_is_refused() {
        let t = Tmp::new();
        let path = journal_path(&t.0);
        assert_eq!(ran_since(&path, OPEN), Ok(false)); // no journal yet
        let mut j = Journal::open(&path).unwrap();
        j.write(OPEN - 86_400, "SessionStart", "", "").unwrap();
        assert_eq!(ran_since(&path, OPEN), Ok(false)); // yesterday only
        j.write(OPEN, "SessionStart", "", "").unwrap();
        assert_eq!(ran_since(&path, OPEN), Ok(true));
        std::fs::write(&path, "{\"ts\":1}\nnot json\n").unwrap();
        assert!(ran_since(&path, OPEN).is_err());
    }
}
