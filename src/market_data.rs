//! Live 1-minute bars for the paper loop: which minutes are closed, what to ask for, dedupe, staleness.
//! shortcut: REST polling, upgrade to the websocket in a later P3 step.

use crate::broker::Alpaca;
use crate::engine::MARKET;
use crate::history::{self, MAX_SYMBOLS, Session};
use crate::strategy::Bar;
use std::collections::BTreeMap;

/// Newest engine::MARKET bar end older than this = stale.
pub const STALE_SECS: i64 = 180;
/// Wait after a minute closes before asking for it.
pub const BAR_DELAY_SECS: i64 = 5;
/// Re-ask the last 2 minutes to catch late bars.
pub const LOOKBACK_SECS: i64 = 120;

/// The start of the current minute. A bar starting at `ts` is closed iff `ts + 60 <= closed_until(now)`.
pub fn closed_until(now: i64) -> i64 {
    now - now.rem_euclid(60)
}

pub struct Feed {
    symbols: Vec<String>,
    open: i64,
    close: i64,
    last: BTreeMap<String, i64>,
    /// Newest sane engine::MARKET bar. SPY prints every minute and is the regime/trend reference, so
    /// it alone decides staleness. A quiet watchlist symbol just has no bars (no signal, no entry);
    /// its stops are bracket legs held by Alpaca, not driven by our bars.
    newest: i64,
    polled_to: Option<i64>,
}

impl Feed {
    /// 1..=MAX_SYMBOLS valid, distinct symbols, including engine::MARKET.
    pub fn new(symbols: Vec<String>, open: i64, close: i64) -> Result<Feed, String> {
        if symbols.is_empty() || symbols.len() > MAX_SYMBOLS {
            return Err(format!(
                "need 1 to {MAX_SYMBOLS} symbols, got {}",
                symbols.len()
            ));
        }
        if let Some(bad) = symbols.iter().find(|s| !history::valid_symbol(s)) {
            return Err(format!("invalid symbol {bad:?}"));
        }
        let mut seen = symbols.clone();
        seen.sort();
        seen.dedup();
        if seen.len() != symbols.len() {
            return Err("duplicate symbols".into());
        }
        if !symbols.iter().any(|s| s == MARKET) {
            return Err(format!("the watchlist must include {MARKET}"));
        }
        if open >= close {
            return Err(format!("session open {open} is not before close {close}"));
        }
        Ok(Feed {
            symbols,
            open,
            close,
            last: BTreeMap::new(),
            // No false staleness at the open.
            newest: open - 60,
            polled_to: None,
        })
    }

    pub fn symbols(&self) -> &[String] {
        &self.symbols
    }

    /// Some((from, to)) when a newly closed minute is due.
    pub fn window(&self, now: i64) -> Option<(i64, i64)> {
        let to = closed_until(now - BAR_DELAY_SECS).min(self.close);
        let from = self
            .polled_to
            .map_or(self.open, |p| self.open.max(p - LOOKBACK_SECS));
        if to <= from || self.polled_to.is_some_and(|p| to <= p) {
            return None;
        }
        Some((from, to))
    }

    /// New bars of watchlist symbols in [open, min(to, close)), sorted by (ts, symbol) as backtest does.
    /// Insane bars are passed on (the engine journals BarRejected) but don't count as fresh data,
    /// and neither do bars of symbols other than engine::MARKET.
    pub fn accept(&mut self, bars: BTreeMap<String, Vec<Bar>>, to: i64) -> Vec<(String, Bar)> {
        let end = to.min(self.close);
        let mut out = Vec::new();
        for (sym, list) in bars {
            if !self.symbols.contains(&sym) {
                continue;
            }
            let mut last = self.last.get(&sym).copied().unwrap_or(i64::MIN);
            let mut list = list;
            list.sort_by_key(|b| b.ts);
            for b in list {
                if b.ts >= self.open && b.ts < end && b.ts > last {
                    last = b.ts;
                    if b.is_sane() && sym == MARKET {
                        self.newest = self.newest.max(b.ts);
                    }
                    out.push((sym.clone(), b));
                }
            }
            if last != i64::MIN {
                self.last.insert(sym, last);
            }
        }
        out.sort_by(|(sa, a), (sb, b)| a.ts.cmp(&b.ts).then_with(|| sa.cmp(sb)));
        self.polled_to = Some(self.polled_to.map_or(to, |p| p.max(to)));
        out
    }

    /// The newest sane engine::MARKET bar ended more than STALE_SECS ago.
    pub fn stale(&self, now: i64) -> bool {
        now - (self.newest + 60) > STALE_SECS
    }
}

/// The one network fn. history::fetch_session already paginates, filters [from, to), sorts and dedups.
pub fn fetch(
    alpaca: &Alpaca,
    symbols: &[String],
    from: i64,
    to: i64,
) -> Result<BTreeMap<String, Vec<Bar>>, String> {
    let s = Session {
        date: String::new(),
        open: from,
        close: to,
    };
    history::fetch_session(alpaca, symbols, &s, &mut || {})
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: i64 = 1_700_000_000 - 1_700_000_000 % 60; // minute-aligned
    const CLOSE: i64 = OPEN + 390 * 60;

    fn syms(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn feed() -> Feed {
        Feed::new(syms(&["SPY", "AAPL"]), OPEN, CLOSE).unwrap()
    }

    fn bar(ts: i64) -> Bar {
        Bar {
            ts,
            open: 10.0,
            high: 11.0,
            low: 9.0,
            close: 10.5,
            volume: 100,
        }
    }

    fn bars(v: &[(&str, &[i64])]) -> BTreeMap<String, Vec<Bar>> {
        v.iter()
            .map(|(s, ts)| (s.to_string(), ts.iter().map(|t| bar(*t)).collect()))
            .collect()
    }

    fn keys(out: &[(String, Bar)]) -> Vec<(String, i64)> {
        out.iter().map(|(s, b)| (s.clone(), b.ts)).collect()
    }

    #[test]
    fn closed_until_floors_to_the_minute() {
        assert_eq!(closed_until(OPEN), OPEN);
        assert_eq!(closed_until(OPEN + 59), OPEN);
        assert_eq!(closed_until(OPEN + 60), OPEN + 60);
        assert_eq!(closed_until(-1), -60);
    }

    #[test]
    fn new_validates_the_watchlist() {
        assert!(Feed::new(vec![], OPEN, CLOSE).is_err());
        let many: Vec<String> = (0..=MAX_SYMBOLS).map(|i| format!("A{i}")).collect();
        assert!(Feed::new(many, OPEN, CLOSE).is_err());
        assert!(Feed::new(syms(&["SPY", "aapl"]), OPEN, CLOSE).is_err());
        assert!(Feed::new(syms(&["SPY", "../X"]), OPEN, CLOSE).is_err());
        assert!(Feed::new(syms(&["SPY", "AAPL", "SPY"]), OPEN, CLOSE).is_err());
        assert!(Feed::new(syms(&["AAPL"]), OPEN, CLOSE).is_err());
        assert!(Feed::new(syms(&["SPY"]), CLOSE, OPEN).is_err());
        assert_eq!(feed().symbols(), &syms(&["SPY", "AAPL"])[..]);
    }

    #[test]
    fn window_walks_closed_minutes() {
        let mut f = feed();
        assert_eq!(f.window(OPEN - 600), None);
        assert_eq!(
            f.window(OPEN + 60),
            None,
            "minute closed but BAR_DELAY not passed"
        );
        assert_eq!(
            f.window(OPEN + 60 + BAR_DELAY_SECS),
            Some((OPEN, OPEN + 60))
        );
        // A failed fetch never calls accept, so the window stays due and grows.
        assert_eq!(f.window(OPEN + 185), Some((OPEN, OPEN + 180)));
        f.accept(BTreeMap::new(), OPEN + 180);
        assert_eq!(f.window(OPEN + 185), None);
        assert_eq!(f.window(OPEN + 240), None);
        assert_eq!(
            f.window(OPEN + 245),
            Some((OPEN + 180 - LOOKBACK_SECS, OPEN + 240))
        );
        // Lookback never reaches before the open.
        let mut g = feed();
        g.accept(BTreeMap::new(), OPEN + 60);
        assert_eq!(g.window(OPEN + 125), Some((OPEN, OPEN + 120)));
        // Capped at the close; nothing after it.
        let mut h = feed();
        assert_eq!(h.window(CLOSE + 3600), Some((OPEN, CLOSE)));
        h.accept(BTreeMap::new(), CLOSE);
        assert_eq!(h.window(CLOSE + 3600), None);
    }

    #[test]
    fn accept_filters_dedupes_and_sorts() {
        let mut f = feed();
        let out = f.accept(
            bars(&[
                ("SPY", &[OPEN + 60, OPEN, OPEN + 120]),
                ("AAPL", &[OPEN, OPEN - 60]),
                ("MSFT", &[OPEN]),
            ]),
            OPEN + 120,
        );
        assert_eq!(
            keys(&out),
            vec![
                ("AAPL".into(), OPEN),
                ("SPY".into(), OPEN),
                ("SPY".into(), OPEN + 60)
            ]
        );
        // Lookback re-fetch: old ts dropped per symbol, each cursor independent.
        let out = f.accept(
            bars(&[
                ("SPY", &[OPEN, OPEN + 60, OPEN + 120]),
                ("AAPL", &[OPEN, OPEN + 60]),
            ]),
            OPEN + 180,
        );
        assert_eq!(
            keys(&out),
            vec![("AAPL".into(), OPEN + 60), ("SPY".into(), OPEN + 120)]
        );
        // Bars at or after the close are outside the session.
        let mut g = feed();
        let out = g.accept(bars(&[("SPY", &[CLOSE - 60, CLOSE])]), CLOSE + 60);
        assert_eq!(keys(&out), vec![("SPY".into(), CLOSE - 60)]);
    }

    #[test]
    fn stale_tracks_the_newest_sane_market_bar() {
        let mut f = feed();
        assert!(!f.stale(OPEN));
        assert!(!f.stale(OPEN + STALE_SECS));
        assert!(f.stale(OPEN + STALE_SECS + 1));
        // Other symbols updating while SPY is silent is still stale.
        f.accept(bars(&[("AAPL", &[OPEN + 60, OPEN + 120])]), OPEN + 180);
        assert!(f.stale(OPEN + STALE_SECS + 1), "SPY silent, AAPL fresh");
        f.accept(bars(&[("SPY", &[OPEN + 120])]), OPEN + 180);
        assert!(!f.stale(OPEN + 180 + STALE_SECS));
        assert!(f.stale(OPEN + 181 + STALE_SECS));
    }

    #[test]
    fn insane_bars_pass_through_once_but_are_not_fresh() {
        let mut f = feed();
        let mut bad = bar(OPEN + 120);
        bad.close = f64::NAN;
        let mut m = BTreeMap::new();
        m.insert("SPY".to_string(), vec![bad]);
        let out = f.accept(m.clone(), OPEN + 180);
        assert_eq!(out.len(), 1, "the engine journals BarRejected");
        assert!(
            f.stale(OPEN + STALE_SECS + 1),
            "a bad bar is not fresh data"
        );
        assert!(
            f.accept(m, OPEN + 240).is_empty(),
            "not re-sent on lookback"
        );
    }
}
