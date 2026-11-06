//! Gap scanner research data (DESIGN §13b): a broad US-stock universe, its daily bars, each day's
//! pre-registered gapper picks, and minute bars for just those picks.
//!
//! Layout under `data/`:
//! - `daily/universe.json`        symbols Alpaca lists, active and inactive (delisted)
//! - `daily/iex/{SYM}.json`       daily bars, raw prices (the gap scan)
//! - `daily/sip-all/{SYM}.json`   daily bars, adjusted for splits and dividends (momentum, §13c)
//! - `scan/iex/{DATE}.json`       that day's picks, best gap first
//! - `history/iex/{SYM}/{DATE}.json` minute bars, shared with `fetch`

use crate::backtest::Day;
use crate::broker::{Alpaca, Api};
use crate::history::{self, Session};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The pre-registered scan (DESIGN §13b). Changing any of these is a new trial.
pub const GAP_MIN: f64 = 0.02;
/// Bigger gaps are mostly reverse splits, buyouts and halts, not momentum.
pub const GAP_MAX: f64 = 0.30;
pub const MIN_PRICE: f64 = 5.0;
/// Average IEX dollar volume over the 20 sessions before; IEX is roughly 2–3% of all volume.
pub const MIN_DOLLAR_VOLUME: f64 = 1_000_000.0;
pub const LIQUIDITY_DAYS: usize = 20;
pub const PICKS_PER_DAY: usize = 10;

const DAILY_BATCH: usize = 100;

/// One downloaded set of daily bars, each in its own folder with its own date range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DailySet {
    feed: &'static str,
    adjustment: &'static str,
    folder: &'static str,
    /// The set keeps its own symbol list (`_symbols.json`) instead of the whole universe.
    listed: bool,
}

/// Raw IEX prices: the gap scan compares today's open with yesterday's real close.
pub const RAW_IEX: DailySet = DailySet {
    feed: history::FEED,
    adjustment: "raw",
    folder: history::FEED,
    listed: false,
};
/// Split- and dividend-adjusted SIP prices, from 2016: returns over months (DESIGN §13c).
pub const ADJUSTED_SIP: DailySet = DailySet {
    feed: "sip",
    adjustment: "all",
    folder: "sip-all",
    listed: false,
};
/// Unadjusted SIP prices, from 2016: what a share really cost that day, for trial 13's price
/// check on insider purchases (DESIGN §7j).
pub const RAW_SIP: DailySet = DailySet {
    feed: "sip",
    adjustment: "raw",
    folder: "sip-raw",
    listed: false,
};
/// The last ~420 days of the same adjusted bars for trial 10 (DESIGN §7f), downloaded fresh each
/// night so every bar shares today's split and dividend adjustment.
pub const ADJUSTED_SIP_LIVE: DailySet = DailySet {
    feed: "sip",
    adjustment: "all",
    folder: "sip-live",
    listed: true,
};
/// Where a refresh downloads before it replaces `sip-live`, so a failed night leaves the old set.
const LIVE_STAGING: DailySet = DailySet {
    folder: "sip-live.tmp",
    ..ADJUSTED_SIP_LIVE
};
const EXCHANGES: [&str; 5] = ["NYSE", "NASDAQ", "AMEX", "ARCA", "BATS"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DailyBar {
    /// New York trading date, YYYY-MM-DD.
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub symbol: String,
    pub prev_close: f64,
    pub gap: f64,
}

/// The day's gappers, using only what is known at the open: yesterday's close, the 20 sessions
/// before today, and today's opening price. `sessions` are all trading dates, oldest first, and
/// `date` must be one of them (not the first). Best gap first, then symbol; at most 10.
pub fn pick_gappers(
    date: &str,
    sessions: &[String],
    daily: &BTreeMap<String, Vec<DailyBar>>,
) -> Vec<Pick> {
    let Some(d) = sessions.iter().position(|s| s == date).filter(|&d| d > 0) else {
        return Vec::new();
    };
    let yesterday = &sessions[d - 1];
    // The 20-session window must lie within the last 30 sessions: no stale liquidity.
    let oldest = &sessions[d.saturating_sub(30)];
    let mut picks: Vec<Pick> = daily
        .iter()
        .filter_map(|(symbol, bars)| {
            let i = bars.binary_search_by(|b| b.date.as_str().cmp(date)).ok()?;
            if i < LIQUIDITY_DAYS {
                return None;
            }
            let window = &bars[i - LIQUIDITY_DAYS..i];
            let prev = window.last()?;
            // Cheap tests first: most symbol-days have no gap. Today: the open only (known at 09:30).
            let open = bars[i].open;
            if !(open.is_finite() && open > 0.0)
                || &prev.date != yesterday
                || prev.close < MIN_PRICE
            {
                return None;
            }
            let gap = open / prev.close - 1.0;
            if !in_gap_range(gap) {
                return None;
            }
            let sane = |b: &DailyBar| {
                [b.open, b.high, b.low, b.close]
                    .iter()
                    .all(|p| p.is_finite() && *p > 0.0)
            };
            if window[0].date < *oldest || !window.iter().all(sane) {
                return None;
            }
            let dollar_volume = window
                .iter()
                .map(|b| b.close * b.volume as f64)
                .sum::<f64>()
                / LIQUIDITY_DAYS as f64;
            (dollar_volume >= MIN_DOLLAR_VOLUME).then(|| Pick {
                symbol: symbol.clone(),
                prev_close: prev.close,
                gap,
            })
        })
        .collect();
    picks.sort_by(|a, b| {
        b.gap
            .total_cmp(&a.gap)
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    picks.truncate(PICKS_PER_DAY);
    picks
}

/// Inclusive at both ends, allowing for float rounding: 13.00 over 10.00 is a +30% gap.
fn in_gap_range(gap: f64) -> bool {
    const EPS: f64 = 1e-9;
    (GAP_MIN - EPS..=GAP_MAX + EPS).contains(&gap)
}

fn range_path(dir: &Path, set: DailySet) -> PathBuf {
    dir.join("daily").join(set.folder).join("_range.json")
}

/// The one date range the daily files cover, so a later scan can't run on short data.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct DailyRange {
    from: String,
    to: String,
}

fn universe_path(dir: &Path) -> PathBuf {
    dir.join("daily").join("universe.json")
}
fn daily_path(dir: &Path, set: DailySet, symbol: &str) -> PathBuf {
    dir.join("daily")
        .join(set.folder)
        .join(format!("{symbol}.json"))
}
fn picks_path(dir: &Path, date: &str) -> PathBuf {
    dir.join("scan")
        .join(history::FEED)
        .join(format!("{date}.json"))
}

/// A common-stock ticker: 1–5 letters, optionally a share class (`BRK.B`). The inactive list also
/// holds CUSIP-like codes for rights and escrows (`0029900E0`), which the bars API rejects.
/// ETFs and ETNs still pass (Alpaca doesn't mark them): a known limit of the universe.
pub fn is_ticker(s: &str) -> bool {
    let (base, class) = s.split_once('.').unwrap_or((s, ""));
    let letters = |t: &str, n: std::ops::RangeInclusive<usize>| {
        n.contains(&t.len()) && t.bytes().all(|b| b.is_ascii_uppercase())
    };
    // Nasdaq's fifth letter W, U or R marks a warrant, unit or right, not the common stock.
    let derivative = base.len() == 5 && base.ends_with(['W', 'U', 'R']);
    letters(base, 1..=5)
        && !derivative
        && (!s.contains('.') || letters(class, 1..=2))
        && history::valid_symbol(s)
}

#[derive(Deserialize)]
struct Asset {
    symbol: String,
    exchange: String,
    #[serde(default)]
    name: String,
}

/// Every listed US stock symbol, active and inactive, on the main exchanges. Inactive ones are
/// what keeps delisted stocks in the past (Alpaca's list is not complete: survivorship remains).
pub fn fetch_universe(alpaca: &Alpaca, dir: &Path) -> Result<usize, String> {
    let mut symbols = Vec::new();
    for status in ["active", "inactive"] {
        let assets: Vec<Asset> = alpaca.get(
            Api::Trading,
            &format!("/v2/assets?status={status}&asset_class=us_equity"),
        )?;
        symbols.extend(
            assets
                .into_iter()
                .filter(|a| EXCHANGES.contains(&a.exchange.as_str()) && is_ticker(&a.symbol))
                .map(|a| a.symbol),
        );
    }
    symbols.sort();
    symbols.dedup();
    history::write_json(&universe_path(dir), &symbols)?;
    Ok(symbols.len())
}

fn asset_names_path(dir: &Path) -> PathBuf {
    dir.join("daily").join("asset_names.json")
}

/// Alpaca's asset name for each symbol already in `universe.json` (DESIGN §7e), which is left
/// unchanged so earlier trials keep their universe. Returns how many symbols got a name.
pub fn fetch_asset_names(alpaca: &Alpaca, dir: &Path) -> Result<usize, String> {
    let universe: Vec<String> = history::read_json(&universe_path(dir))
        .map_err(|e| format!("{e}: run `bmv-trader universe` first"))?;
    let mut names = BTreeMap::new();
    for status in ["active", "inactive"] {
        let assets: Vec<Asset> = alpaca.get(
            Api::Trading,
            &format!("/v2/assets?status={status}&asset_class=us_equity"),
        )?;
        for a in assets {
            if !a.name.is_empty() && universe.binary_search(&a.symbol).is_ok() {
                names.entry(a.symbol).or_insert(a.name);
            }
        }
    }
    history::write_json(&asset_names_path(dir), &names)?;
    Ok(names.len())
}

/// The names `fetch_asset_names` saved.
pub fn read_asset_names(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    history::read_json(&asset_names_path(dir))
        .map_err(|e| format!("{e}: run `bmv-trader asset-names` first"))
}

#[derive(Deserialize)]
struct RawDaily {
    t: String,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: u64,
}

#[derive(Deserialize)]
struct DailyPage {
    #[serde(default)]
    bars: Option<BTreeMap<String, Vec<RawDaily>>>,
    next_page_token: Option<String>,
}

/// The symbol in Alpaca's `{"message":"invalid symbol: X"}` refusal, if that's what `err` is.
fn rejected_symbol(err: &str) -> Option<&str> {
    let rest = &err[err.find("invalid symbol: ")? + "invalid symbol: ".len()..];
    let end = rest
        .find(|c: char| c == '"' || c.is_whitespace())
        .unwrap_or(rest.len());
    Some(&rest[..end]).filter(|s| !s.is_empty())
}

/// A daily bar's time is New York midnight; its New York date is the trading date.
fn ny_date(t: &str) -> Result<String, String> {
    Ok(DateTime::parse_from_rfc3339(t)
        .map_err(|e| format!("daily bar time {t:?}: {e}"))?
        .with_timezone(&chrono_tz::America::New_York)
        .date_naive()
        .to_string())
}

/// Daily bars for the whole universe in [from, to]. Resumable: symbols with a file are skipped.
/// A symbol with no bars gets an empty file, so it isn't asked for again.
pub fn fetch_daily(
    alpaca: &Alpaca,
    dir: &Path,
    set: DailySet,
    from: &str,
    to: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<usize, String> {
    let universe = symbols_of(dir, set)?;
    // Resuming skips symbols that have a file, so every file must cover the same range.
    let range = DailyRange {
        from: from.into(),
        to: to.into(),
    };
    match history::read_json::<DailyRange>(&range_path(dir, set)) {
        Ok(r) if r != range => {
            return Err(format!(
                "daily bars already cover {}..{}; delete data/daily/{} to fetch {from}..{to}",
                r.from, r.to, set.folder
            ));
        }
        _ => history::write_json(&range_path(dir, set), &range)?,
    }
    let todo: Vec<&String> = universe
        .iter()
        .filter(|s| !daily_path(dir, set, s).exists())
        .collect();
    let mut done = 0;
    for batch in todo.chunks(DAILY_BATCH) {
        let mut names: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
        let mut got: BTreeMap<String, Vec<DailyBar>> = BTreeMap::new();
        let mut token: Option<String> = None;
        let mut pages = 0;
        loop {
            if names.is_empty() {
                break;
            }
            pages += 1;
            if pages > 1_000 {
                return Err("more than 1,000 pages for one batch: Alpaca is looping".into());
            }
            let mut path = format!(
                "/v2/stocks/bars?symbols={}&timeframe=1Day&start={from}&end={to}&feed={}&adjustment={}&limit=10000",
                history::encode(&names.join(",")),
                set.feed,
                set.adjustment
            );
            if let Some(t) = &token {
                path.push_str(&format!("&page_token={}", history::encode(t)));
            }
            std::thread::sleep(std::time::Duration::from_millis(history::CALL_SPACING_MS));
            let page: DailyPage = match alpaca.get(Api::MarketData, &path) {
                Ok(p) => p,
                // Alpaca names one symbol it won't serve: drop it (it gets no bars) and retry the batch.
                Err(e) => {
                    match rejected_symbol(&e).filter(|r| names.contains(r) && token.is_none()) {
                        Some(bad) => {
                            let bad = bad.to_string();
                            names.retain(|n| *n != bad);
                            got.clear();
                            continue;
                        }
                        None => return Err(e),
                    }
                }
            };
            for (sym, raw) in page.bars.unwrap_or_default() {
                let bars = got.entry(sym).or_default();
                for r in raw {
                    bars.push(DailyBar {
                        date: ny_date(&r.t)?,
                        open: r.o,
                        high: r.h,
                        low: r.l,
                        close: r.c,
                        volume: r.v,
                    });
                }
            }
            match page.next_page_token.filter(|t| !t.is_empty()) {
                Some(t) if Some(&t) != token.as_ref() => token = Some(t),
                Some(_) => return Err("Alpaca repeated a page token".into()),
                None => break,
            }
        }
        for sym in batch {
            let mut bars = got.remove(sym.as_str()).unwrap_or_default();
            bars.sort_by(|a, b| a.date.cmp(&b.date));
            bars.dedup_by(|a, b| a.date == b.date);
            history::write_json(&daily_path(dir, set, sym), &bars)?;
        }
        done += batch.len();
        progress(done, todo.len());
    }
    Ok(todo.len())
}

/// The universe's daily bars in `set`; symbols without bars are left out.
fn load_daily(dir: &Path, set: DailySet) -> Result<BTreeMap<String, Vec<DailyBar>>, String> {
    let mut daily = BTreeMap::new();
    for_each_daily(dir, set, &mut |s, bars| {
        daily.insert(s.to_string(), bars);
        Ok(())
    })?;
    Ok(daily)
}

/// Called with each symbol and its bars.
pub type OnDaily<'a> = dyn FnMut(&str, Vec<DailyBar>) -> Result<(), String> + 'a;

/// Each universe symbol with bars in `set`, one at a time, so a long history needn't fit in memory.
/// Errors if a symbol has no file (a partial download).
pub fn for_each_daily(dir: &Path, set: DailySet, f: &mut OnDaily<'_>) -> Result<(), String> {
    for s in symbols_of(dir, set)? {
        let bars: Vec<DailyBar> = history::read_json(&daily_path(dir, set, &s))
            .map_err(|e| format!("{e}: run `bmv-trader fetch-daily` first"))?;
        if !bars.is_empty() {
            f(&s, bars)?;
        }
    }
    Ok(())
}

/// The symbols `set` holds: its own list, or the whole universe.
fn symbols_of(dir: &Path, set: DailySet) -> Result<Vec<String>, String> {
    if set.listed {
        history::read_json(&dir.join("daily").join(set.folder).join("_symbols.json"))
            .map_err(|e| format!("{e}: run `bmv-trader desk-run` to download it"))
    } else {
        read_universe(dir)
    }
}

/// Every symbol `bmv-trader universe` listed.
pub fn read_universe(dir: &Path) -> Result<Vec<String>, String> {
    history::read_json(&universe_path(dir))
        .map_err(|e| format!("{e}: run `bmv-trader universe` first"))
}

/// Downloads `symbols` over [from, to] into a staging folder, then swaps it in as `sip-live`.
/// A failure leaves yesterday's set in place.
pub fn refresh_live(
    alpaca: &Alpaca,
    dir: &Path,
    symbols: &[String],
    from: &str,
    to: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<usize, String> {
    let staging = dir.join("daily").join(LIVE_STAGING.folder);
    let live = dir.join("daily").join(ADJUSTED_SIP_LIVE.folder);
    match std::fs::remove_dir_all(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot clear {}: {e}", staging.display())),
    }
    history::write_json(&staging.join("_symbols.json"), &symbols)?;
    let n = fetch_daily(alpaca, dir, LIVE_STAGING, from, to, progress)?;
    match std::fs::remove_dir_all(&live) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot replace {}: {e}", live.display())),
    }
    std::fs::rename(&staging, &live)
        .map_err(|e| format!("cannot move {} into place: {e}", staging.display()))?;
    Ok(n)
}

/// One symbol's bars in `set`.
pub fn read_daily(dir: &Path, set: DailySet, symbol: &str) -> Result<Vec<DailyBar>, String> {
    history::read_json(&daily_path(dir, set, symbol))
        .map_err(|e| format!("{e}: run `bmv-trader fetch-daily` first"))
}

/// The date range `set` was downloaded for.
pub fn daily_range(dir: &Path, set: DailySet) -> Result<(String, String), String> {
    let r: DailyRange = history::read_json(&range_path(dir, set))
        .map_err(|e| format!("{e}: run `bmv-trader fetch-daily` first"))?;
    Ok((r.from, r.to))
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub days: usize,
    pub picks: usize,
    /// Picks Alpaca had no minute bars for: kept as days without trades.
    pub no_minute_bars: usize,
}

/// For each session in [from, to]: picks the gappers, saves them, and fetches their minute bars.
/// Resumable: a day whose picks and bars exist is skipped.
pub fn scan_range(
    alpaca: &Alpaca,
    dir: &Path,
    from: &str,
    to: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<ScanReport, String> {
    // Sessions from 60 days earlier, so the first day has its 30-session lookback.
    let start = (history::canonical_date(from)? - chrono::Days::new(60)).to_string();
    let covered: DailyRange = history::read_json(&range_path(dir, RAW_IEX))
        .map_err(|e| format!("{e}: run `bmv-trader fetch-daily` first"))?;
    if covered.from.as_str() > start.as_str() || covered.to.as_str() < to {
        return Err(format!(
            "daily bars cover {}..{}, but scanning {from}..{to} needs {start}..{to}",
            covered.from, covered.to
        ));
    }
    let daily = load_daily(dir, RAW_IEX)?;
    let rows: Vec<history::CalendarRow> = alpaca.get(
        Api::Trading,
        &format!("/v2/calendar?start={start}&end={to}"),
    )?;
    let sessions = history::parse_calendar(&rows)?;
    let dates: Vec<String> = sessions.iter().map(|s| s.date.clone()).collect();
    let mut report = ScanReport::default();
    let mut wait =
        || std::thread::sleep(std::time::Duration::from_millis(history::CALL_SPACING_MS));
    for s in sessions.iter().filter(|s| s.date.as_str() >= from) {
        history::write_json(&history::session_path(dir, &s.date), s)?;
        let picks = pick_gappers(&s.date, &dates, &daily);
        history::write_json(&picks_path(dir, &s.date), &picks)?;
        report.days += 1;
        report.picks += picks.len();
        let missing: Vec<String> = picks
            .iter()
            .map(|p| p.symbol.clone())
            .filter(|sym| !history::bars_path(dir, sym, &s.date).exists())
            .collect();
        if !missing.is_empty() {
            let mut got = history::fetch_session(alpaca, &missing, s, &mut wait)?;
            for sym in &missing {
                let bars = got.remove(sym).unwrap_or_default();
                if bars.is_empty() {
                    report.no_minute_bars += 1;
                }
                history::write_json(&history::bars_path(dir, sym, &s.date), &bars)?;
            }
        }
        progress(&s.date);
    }
    Ok(report)
}

/// Scanned days in [from, to] as backtest days: each day's picks, their minute bars, and their
/// previous close. Any missing file is an error, so a backtest never runs on a partial scan.
pub fn load_scan_range(dir: &Path, from: &str, to: &str) -> Result<Vec<Day>, String> {
    let sessions_dir = dir.join("scan").join(history::FEED);
    let mut dates: Vec<String> = std::fs::read_dir(&sessions_dir)
        .map_err(|e| {
            format!(
                "no scan in {}: {e}: run `bmv-trader scan`",
                sessions_dir.display()
            )
        })?
        .filter_map(|e| {
            e.ok()?
                .file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .filter(|d| d.as_str() >= from && d.as_str() <= to)
        .collect();
    dates.sort();
    let sessions: Vec<String> = std::fs::read_dir(dir.join("history").join("sessions"))
        .map_err(|e| format!("no sessions: {e}"))?
        .filter_map(|e| {
            e.ok()?
                .file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .filter(|d| d.as_str() >= from && d.as_str() <= to)
        .collect();
    if let Some(missing) = sessions.iter().find(|d| !dates.contains(d)) {
        return Err(format!(
            "session {missing} has no scan: run `bmv-trader scan` over {from}..{to}"
        ));
    }
    if dates.is_empty() {
        return Err(format!(
            "no scanned days in {from}..{to}: run `bmv-trader scan`"
        ));
    }
    dates
        .iter()
        .map(|date| {
            let s: Session = history::read_json(&history::session_path(dir, date))?;
            let picks: Vec<Pick> = history::read_json(&picks_path(dir, date))?;
            let mut bars = Vec::new();
            let mut prev_close = BTreeMap::new();
            for p in picks {
                let b: Vec<crate::strategy::Bar> =
                    history::read_json(&history::bars_path(dir, &p.symbol, date))?;
                bars.extend(b.into_iter().map(|b| (p.symbol.clone(), b)));
                prev_close.insert(p.symbol, p.prev_close);
            }
            Ok(Day {
                date: s.date,
                open: s.open,
                close: s.close,
                bars,
                prev_close,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(date: &str, open: f64, close: f64, volume: u64) -> DailyBar {
        DailyBar {
            date: date.into(),
            open,
            high: open.max(close) + 0.1,
            low: open.min(close) - 0.1,
            close,
            volume,
        }
    }

    /// 25 sessions named s00..s24; `bars(open_today)`: 24 days at 10.00 with 200k shares
    /// ($2M a day), then today's open.
    fn setup(today_open: f64) -> (Vec<String>, Vec<DailyBar>) {
        let sessions: Vec<String> = (0..25).map(|i| format!("2024-01-{:02}", i + 1)).collect();
        let mut bars: Vec<DailyBar> = sessions[..24]
            .iter()
            .map(|s| d(s, 10.0, 10.0, 200_000))
            .collect();
        bars.push(d(&sessions[24], today_open, today_open, 200_000));
        (sessions, bars)
    }

    fn picks_for(symbols: &[(&str, Vec<DailyBar>)], sessions: &[String]) -> Vec<Pick> {
        let daily: BTreeMap<String, Vec<DailyBar>> = symbols
            .iter()
            .map(|(s, b)| (s.to_string(), b.clone()))
            .collect();
        pick_gappers(&sessions[24], sessions, &daily)
    }

    #[test]
    fn picks_liquid_gappers_between_two_and_thirty_percent_best_first() {
        let (sessions, small) = setup(10.25); // +2.5%
        let (_, big) = setup(11.0); // +10%
        let (_, tiny) = setup(10.1); // +1%: not a gap
        let (_, split) = setup(40.0); // +300%: a reverse split, not momentum
        let got = picks_for(
            &[("AAA", small), ("BBB", big), ("CCC", tiny), ("DDD", split)],
            &sessions,
        );
        let names: Vec<&str> = got.iter().map(|p| p.symbol.as_str()).collect();
        assert_eq!(names, ["BBB", "AAA"]);
        assert!((got[0].gap - 0.10).abs() < 1e-9);
        assert_eq!(got[0].prev_close, 10.0);
    }

    #[test]
    fn skips_cheap_illiquid_new_or_stale_symbols() {
        let (sessions, ok) = setup(11.0);
        let cheap: Vec<DailyBar> = ok
            .iter()
            .map(|b| DailyBar {
                open: b.open / 4.0,
                high: b.high / 4.0,
                low: b.low / 4.0,
                close: b.close / 4.0,
                ..b.clone()
            })
            .collect();
        let thin: Vec<DailyBar> = ok
            .iter()
            .map(|b| DailyBar {
                volume: 1_000,
                ..b.clone()
            })
            .collect();
        let new = ok[10..].to_vec(); // only 14 days of history
        let mut missed_yesterday = ok.clone();
        missed_yesterday.remove(23);
        let got = picks_for(
            &[
                ("CHP", cheap),
                ("THN", thin),
                ("NEW", new),
                ("GAP", missed_yesterday),
                ("OK", ok),
            ],
            &sessions,
        );
        let names: Vec<&str> = got.iter().map(|p| p.symbol.as_str()).collect();
        assert_eq!(names, ["OK"]);
    }

    #[test]
    fn exact_two_and_thirty_percent_gaps_count() {
        assert!(in_gap_range(13.0 / 10.0 - 1.0));
        assert!(in_gap_range(10.2 / 10.0 - 1.0));
        assert!(!in_gap_range(10.19 / 10.0 - 1.0));
        assert!(!in_gap_range(13.01 / 10.0 - 1.0));
    }

    #[test]
    fn a_liquidity_window_older_than_thirty_sessions_is_skipped() {
        let cal: Vec<String> = (0..50).map(|i| format!("D{i:03}")).collect();
        let on = |i: usize, open: f64| d(&cal[i], open, 10.0, 200_000);
        // 19 days long ago, then yesterday (D048) and a gap today (D049)
        let mut stale: Vec<DailyBar> = (0..19).map(|i| on(i, 10.0)).collect();
        stale.extend([on(48, 10.0), on(49, 11.0)]);
        let fresh: Vec<DailyBar> = (29..49)
            .map(|i| on(i, 10.0))
            .chain([on(49, 11.0)])
            .collect();
        let daily: BTreeMap<String, Vec<DailyBar>> =
            [("OLD".to_string(), stale), ("NEW".to_string(), fresh)].into();
        let got: Vec<String> = pick_gappers(&cal[49], &cal, &daily)
            .into_iter()
            .map(|p| p.symbol)
            .collect();
        assert_eq!(got, ["NEW"]);
    }

    #[test]
    fn at_most_ten_and_never_on_the_first_session() {
        let (sessions, bars) = setup(11.0);
        let many: Vec<(String, Vec<DailyBar>)> = (0..15)
            .map(|i| (format!("S{i:02}"), bars.clone()))
            .collect();
        let daily: BTreeMap<String, Vec<DailyBar>> = many.into_iter().collect();
        assert_eq!(pick_gappers(&sessions[24], &sessions, &daily).len(), 10);
        assert!(pick_gappers(&sessions[0], &sessions, &daily).is_empty());
        assert!(pick_gappers("1999-01-01", &sessions, &daily).is_empty());
    }

    #[test]
    fn only_real_tickers_and_alpacas_refusals_are_parsed() {
        for ok in ["A", "AAPL", "GOOGL", "BRK.B", "BF.A"] {
            assert!(is_ticker(ok), "{ok}");
        }
        for bad in [
            "ACAHW",
            "ACAHU",
            "ACAHR",
            "0029900E0",
            "003CVR016",
            "ABCDEF",
            "BRK.",
            ".B",
            "AB.CDE",
            "aapl",
            "",
        ] {
            assert!(!is_ticker(bad), "{bad}");
        }
        let e = r#"Alpaca /v2/stocks/bars?... HTTP 400: {"message":"invalid symbol: XYZQ"}"#;
        assert_eq!(rejected_symbol(e), Some("XYZQ"));
        assert_eq!(rejected_symbol("HTTP 500: oops"), None);
    }

    #[test]
    fn daily_bar_times_become_new_york_dates() {
        assert_eq!(ny_date("2024-01-02T05:00:00Z").unwrap(), "2024-01-02"); // EST midnight
        assert_eq!(ny_date("2024-07-01T04:00:00Z").unwrap(), "2024-07-01"); // EDT midnight
        assert!(ny_date("yesterday").is_err());
    }
}
