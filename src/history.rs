//! Historical 1-minute bars from Alpaca for research (DESIGN §13), cached under `data/history/`.
//! The feed is always explicit (`iex`, the same feed the live engine trades on) and part of the
//! cache path, so backtests never silently mix feeds. Sessions come from Alpaca's calendar, so
//! half-days have their real close. Downloads resume: a readable cached (symbol, day) is never
//! fetched again. A range can only be loaded after a fetch of it (for those symbols) completed.
//!
//! Layout: `history/sessions/<date>.json`, `history/<feed>/<SYMBOL>/<date>.json`, and
//! `history/<feed>/fetched.jsonl` (one line per completed fetch).

use crate::backtest::Day;
use crate::broker::{Alpaca, Api};
use crate::strategy::Bar;
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::America::New_York;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FEED: &str = "iex";
/// Also Alpaca's free websocket limit, and keeps the bars URL a sane length.
pub const MAX_SYMBOLS: usize = 30;
/// Free plan: 200 historical calls a minute. One call every 350 ms stays under it.
pub const CALL_SPACING_MS: u64 = 350;

type BarsBySymbol = BTreeMap<String, Vec<Bar>>;

/// A ticker: 1–10 of A–Z, 0–9 and '.', not starting with '.' and without "..", so it is always
/// a safe single path component (BRK.B is fine, `../x` is not).
pub fn valid_symbol(s: &str) -> bool {
    (1..=10).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'.')
        && !s.starts_with('.')
        && !s.contains("..")
}

fn check_symbols(symbols: &[String]) -> Result<(), String> {
    if symbols.is_empty() || symbols.len() > MAX_SYMBOLS {
        return Err(format!(
            "need 1 to {MAX_SYMBOLS} symbols, got {}",
            symbols.len()
        ));
    }
    if let Some(bad) = symbols.iter().find(|s| !valid_symbol(s)) {
        return Err(format!("invalid symbol {bad:?}"));
    }
    let mut seen = symbols.to_vec();
    seen.sort();
    seen.dedup();
    if seen.len() != symbols.len() {
        return Err("duplicate symbols".into());
    }
    Ok(())
}

/// Only canonical YYYY-MM-DD: dates are compared as strings, and "2024-1-2" would sort wrongly.
pub fn canonical_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .filter(|d| d.format("%Y-%m-%d").to_string() == s)
        .ok_or_else(|| format!("date must be YYYY-MM-DD, got {s:?}"))
}

fn check_range(start: &str, end: &str) -> Result<(), String> {
    if canonical_date(start)? > canonical_date(end)? {
        return Err(format!("start {start} is after end {end}"));
    }
    Ok(())
}

/// A regular session; times are Unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub date: String,
    pub open: i64,
    pub close: i64,
}

/// One row of Alpaca's `/v2/calendar` (open and close are New York local times).
#[derive(Clone, Debug, Deserialize)]
pub struct CalendarRow {
    pub date: String,
    pub open: String,
    pub close: String,
}

pub fn parse_calendar(rows: &[CalendarRow]) -> Result<Vec<Session>, String> {
    rows.iter()
        .map(|r| {
            let day = canonical_date(&r.date).map_err(|e| format!("calendar: {e}"))?;
            let at = |hm: &str| -> Result<i64, String> {
                let t = NaiveTime::parse_from_str(hm, "%H:%M")
                    .map_err(|e| format!("calendar time {hm:?}: {e}"))?;
                New_York
                    .from_local_datetime(&day.and_time(t))
                    .single()
                    .map(|dt| dt.timestamp())
                    .ok_or_else(|| format!("{} {hm} is not a single New York time", r.date))
            };
            let (open, close) = (at(&r.open)?, at(&r.close)?);
            if close <= open {
                return Err(format!(
                    "calendar {}: close {} not after open {}",
                    r.date, r.close, r.open
                ));
            }
            Ok(Session {
                date: r.date.clone(),
                open,
                close,
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct RawBar {
    t: String,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: u64,
}

#[derive(Debug, Deserialize)]
struct BarsPage {
    #[serde(default)]
    bars: Option<BTreeMap<String, Vec<RawBar>>>,
    next_page_token: Option<String>,
}

/// One page of `/v2/stocks/bars` as bars; those outside the session are dropped.
fn bars_from_page(page: BarsPage, s: &Session) -> Result<(BarsBySymbol, Option<String>), String> {
    let mut out = BarsBySymbol::new();
    for (symbol, raw) in page.bars.unwrap_or_default() {
        for r in raw {
            let ts = DateTime::parse_from_rfc3339(&r.t)
                .map_err(|e| format!("{symbol} bar time {:?}: {e}", r.t))?
                .timestamp();
            if ts >= s.open && ts < s.close {
                out.entry(symbol.clone()).or_default().push(Bar {
                    ts,
                    open: r.o,
                    high: r.h,
                    low: r.l,
                    close: r.c,
                    volume: r.v,
                });
            }
        }
    }
    Ok((out, page.next_page_token.filter(|t| !t.is_empty())))
}

#[cfg(test)]
fn parse_bars_page(json: &str, s: &Session) -> Result<(BarsBySymbol, Option<String>), String> {
    bars_from_page(
        serde_json::from_str(json).map_err(|e| format!("bars page: {e}"))?,
        s,
    )
}

fn utc(ts: i64) -> Result<String, String> {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .ok_or_else(|| format!("timestamp {ts} out of range"))
}

/// Percent-encodes everything except unreserved URL characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Downloads one session's bars for `symbols`, following pagination. `wait` runs before each call.
fn fetch_session(
    alpaca: &Alpaca,
    symbols: &[String],
    s: &Session,
    wait: &mut dyn FnMut(),
) -> Result<BarsBySymbol, String> {
    let mut all = BarsBySymbol::new();
    let mut token: Option<String> = None;
    loop {
        let mut path = format!(
            "/v2/stocks/bars?symbols={}&timeframe=1Min&start={}&end={}&feed={FEED}&adjustment=raw&limit=10000",
            encode(&symbols.join(",")),
            encode(&utc(s.open)?),
            encode(&utc(s.close)?),
        );
        if let Some(t) = &token {
            path.push_str(&format!("&page_token={}", encode(t)));
        }
        wait();
        let page: BarsPage = alpaca.get(Api::MarketData, &path)?;
        let (bars, next) = bars_from_page(page, s)?;
        for (sym, mut b) in bars {
            all.entry(sym).or_default().append(&mut b);
        }
        match next {
            Some(t) if Some(&t) != token.as_ref() => token = Some(t),
            Some(_) => return Err(format!("{}: Alpaca repeated a page token", s.date)),
            None => break,
        }
    }
    for b in all.values_mut() {
        b.sort_by_key(|b| b.ts);
        b.dedup_by_key(|b| b.ts);
    }
    Ok(all)
}

/// An open session with no bars at all means a wrong symbol or a data problem, not a quiet day.
fn require_bars(symbol: &str, date: &str, bars: Option<Vec<Bar>>) -> Result<Vec<Bar>, String> {
    match bars {
        Some(b) if !b.is_empty() => Ok(b),
        _ => Err(format!(
            "no {FEED} bars for {symbol} on {date}: check the symbol, then fetch again"
        )),
    }
}

fn session_path(dir: &Path, date: &str) -> PathBuf {
    dir.join("history")
        .join("sessions")
        .join(format!("{date}.json"))
}

fn bars_path(dir: &Path, symbol: &str, date: &str) -> PathBuf {
    dir.join("history")
        .join(FEED)
        .join(symbol)
        .join(format!("{date}.json"))
}

fn fetched_path(dir: &Path) -> PathBuf {
    dir.join("history").join(FEED).join("fetched.jsonl")
}

/// Writes via a per-process temp file, synced, then renamed: an interrupted or concurrent run
/// never leaves a half file under the real name.
fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let json = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()
    };
    write().map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename into {}: {e}", path.display()))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("corrupt cache file {}: {e}", path.display()))
}

/// A completed fetch of `symbols` over [from, to].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Fetched {
    from: String,
    to: String,
    symbols: Vec<String>,
}

fn fetched_ranges(dir: &Path) -> Result<Vec<Fetched>, String> {
    let path = fetched_path(dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("corrupt {}: {e}", path.display())))
        .collect()
}

fn record_fetched(dir: &Path, f: &Fetched) -> Result<(), String> {
    let path = fetched_path(dir);
    let line = serde_json::to_string(f).map_err(|e| e.to_string())? + "\n";
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchReport {
    pub sessions: usize,
    pub downloaded: usize,
    pub cached: usize,
}

/// Downloads every session in [start, end] (YYYY-MM-DD) for `symbols` into `dir`, skipping what is
/// already cached and readable, then records the range as complete.
pub fn fetch_range(
    alpaca: &Alpaca,
    dir: &Path,
    symbols: &[String],
    start: &str,
    end: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<FetchReport, String> {
    check_symbols(symbols)?;
    check_range(start, end)?;
    let path = format!("/v2/calendar?start={}&end={}", encode(start), encode(end));
    let rows: Vec<CalendarRow> = alpaca.get(Api::Trading, &path)?;
    let sessions = parse_calendar(&rows)?;
    let mut report = FetchReport {
        sessions: sessions.len(),
        ..FetchReport::default()
    };
    let mut wait = || std::thread::sleep(std::time::Duration::from_millis(CALL_SPACING_MS));
    for s in &sessions {
        write_json(&session_path(dir, &s.date), s)?;
        let missing: Vec<String> = symbols
            .iter()
            .filter(|sym| read_json::<Vec<Bar>>(&bars_path(dir, sym, &s.date)).is_err())
            .cloned()
            .collect();
        report.cached += symbols.len() - missing.len();
        if missing.is_empty() {
            continue;
        }
        let mut got = fetch_session(alpaca, &missing, s, &mut wait)?;
        for sym in &missing {
            let bars = require_bars(sym, &s.date, got.remove(sym))?;
            write_json(&bars_path(dir, sym, &s.date), &bars)?;
        }
        report.downloaded += missing.len();
        progress(&s.date);
    }
    record_fetched(
        dir,
        &Fetched {
            from: start.into(),
            to: end.into(),
            symbols: symbols.to_vec(),
        },
    )?;
    Ok(report)
}

/// Loads cached sessions in [start, end] for `symbols`. The range must lie inside one completed
/// fetch that included every symbol, so a backtest never runs on silently missing days.
pub fn load_range(
    dir: &Path,
    symbols: &[String],
    start: &str,
    end: &str,
) -> Result<Vec<Day>, String> {
    check_symbols(symbols)?;
    check_range(start, end)?;
    let covered = fetched_ranges(dir)?.iter().any(|f| {
        f.from.as_str() <= start
            && f.to.as_str() >= end
            && symbols.iter().all(|s| f.symbols.contains(s))
    });
    if !covered {
        return Err(format!(
            "{start}..{end} for {} hasn't been fetched completely: run fetch for that range",
            symbols.join(",")
        ));
    }
    let sessions_dir = dir.join("history").join("sessions");
    let mut dates: Vec<String> = std::fs::read_dir(&sessions_dir)
        .map_err(|e| format!("no downloaded history in {}: {e}", sessions_dir.display()))?
        .filter_map(|e| {
            e.ok()?
                .file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .filter(|d| d.as_str() >= start && d.as_str() <= end)
        .collect();
    dates.sort();
    dates
        .iter()
        .map(|date| {
            let s: Session = read_json(&session_path(dir, date))?;
            if &s.date != date {
                return Err(format!(
                    "{}: holds session {}",
                    session_path(dir, date).display(),
                    s.date
                ));
            }
            let mut bars = Vec::new();
            for sym in symbols {
                let b: Vec<Bar> = read_json(&bars_path(dir, sym, date))?;
                bars.extend(b.into_iter().map(|b| (sym.clone(), b)));
            }
            Ok(Day {
                date: s.date,
                open: s.open,
                close: s.close,
                bars,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        // 2024-01-02: 09:30–16:00 New York = 14:30–21:00 UTC (EST)
        Session {
            date: "2024-01-02".into(),
            open: 1_704_205_800,
            close: 1_704_229_200,
        }
    }

    fn syms(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn symbols_must_be_safe_tickers() {
        for ok in ["SPY", "BRK.B", "A", "QQQ3"] {
            assert!(valid_symbol(ok), "{ok}");
        }
        for bad in [
            "",
            "spy",
            "../x",
            "..",
            ".SPY",
            "A..B",
            "A/B",
            "A\\B",
            "TOOLONGSYMB",
            "SP Y",
        ] {
            assert!(!valid_symbol(bad), "{bad:?}");
        }
        assert!(check_symbols(&syms(&["SPY", "QQQ"])).is_ok());
        assert!(check_symbols(&[]).is_err());
        assert!(check_symbols(&syms(&["SPY", "SPY"])).is_err());
        assert!(check_symbols(&vec!["SPY".to_string(); MAX_SYMBOLS + 1]).is_err());
    }

    #[test]
    fn dates_must_be_canonical() {
        assert!(canonical_date("2024-01-02").is_ok());
        for bad in [
            "2024-1-2",
            "2024-01-2",
            "24-01-02",
            "2024/01/02",
            "2024-02-30",
            "",
        ] {
            assert!(canonical_date(bad).is_err(), "{bad:?}");
        }
        assert!(check_range("2024-01-02", "2024-01-01").is_err());
        assert!(check_range("2024-01-01", "2024-01-01").is_ok());
    }

    #[test]
    fn calendar_times_are_new_york_local_including_dst_and_half_days() {
        let rows: Vec<CalendarRow> = serde_json::from_str(
            r#"[{"date":"2024-01-02","open":"09:30","close":"16:00","session_open":"0400","session_close":"2000"},
                {"date":"2024-07-03","open":"09:30","close":"13:00"}]"#,
        )
        .unwrap();
        let s = parse_calendar(&rows).unwrap();
        assert_eq!(s[0], session());
        // July: EDT (UTC−4), and the half-day closes at 13:00
        assert_eq!((s[1].open, s[1].close), (1_720_013_400, 1_720_026_000));
    }

    #[test]
    fn calendar_rejects_garbage() {
        let bad = |date: &str, open: &str, close: &str| {
            parse_calendar(&[CalendarRow {
                date: date.into(),
                open: open.into(),
                close: close.into(),
            }])
            .is_err()
        };
        assert!(bad("2024-01-02", "9h30", "16:00"));
        assert!(bad("2024-01-02", "16:00", "09:30"));
        assert!(bad("Jan 2", "09:30", "16:00"));
        assert!(bad("2024-1-2", "09:30", "16:00"));
    }

    /// Shape of a real Alpaca response (fields trimmed to what's used plus extras that must be ignored).
    const PAGE: &str = r#"{"bars":{"SPY":[
        {"t":"2024-01-02T14:29:00Z","o":1,"h":1,"l":1,"c":1,"v":1,"n":1,"vw":1},
        {"t":"2024-01-02T14:30:00Z","o":472.16,"h":472.4,"l":471.9,"c":472.1,"v":12345,"n":200,"vw":472.2},
        {"t":"2024-01-02T14:31:00Z","o":472.1,"h":472.3,"l":472.0,"c":472.25,"v":8000,"n":150,"vw":472.1}],
        "QQQ":[{"t":"2024-01-02T21:00:00Z","o":1,"h":1,"l":1,"c":1,"v":1}]},
        "next_page_token":"U1BZfDIwMjQ="}"#;

    #[test]
    fn parses_a_bars_page_and_drops_bars_outside_the_session() {
        let (bars, next) = parse_bars_page(PAGE, &session()).unwrap();
        assert_eq!(bars["SPY"].len(), 2, "pre-open bar dropped");
        assert!(!bars.contains_key("QQQ"), "bar at the close dropped");
        assert_eq!(
            bars["SPY"][0],
            Bar {
                ts: 1_704_205_800,
                open: 472.16,
                high: 472.4,
                low: 471.9,
                close: 472.1,
                volume: 12345
            }
        );
        assert_eq!(next.as_deref(), Some("U1BZfDIwMjQ="));
    }

    #[test]
    fn an_empty_page_is_fine_and_bad_data_is_an_error() {
        let (bars, next) =
            parse_bars_page(r#"{"bars":null,"next_page_token":null}"#, &session()).unwrap();
        assert!(bars.is_empty() && next.is_none());
        assert!(
            parse_bars_page(
                r#"{"bars":{"SPY":[{"t":"yesterday","o":1,"h":1,"l":1,"c":1,"v":1}]}}"#,
                &session()
            )
            .is_err()
        );
        assert!(
            parse_bars_page(
                r#"{"bars":{"SPY":[{"t":"2024-01-02T14:30:00Z","o":1}]}}"#,
                &session()
            )
            .is_err()
        );
    }

    #[test]
    fn an_open_session_without_bars_is_an_error() {
        assert!(require_bars("SPY", "2024-01-02", None).is_err());
        assert!(require_bars("SPY", "2024-01-02", Some(vec![])).is_err());
        let (bars, _) = parse_bars_page(PAGE, &session()).unwrap();
        assert_eq!(
            require_bars("SPY", "2024-01-02", bars.get("SPY").cloned())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn urls_are_encoded() {
        assert_eq!(encode("SPY,BRK.B"), "SPY%2CBRK.B");
        assert_eq!(encode("2024-01-02T14:30:00Z"), "2024-01-02T14%3A30%3A00Z");
        assert_eq!(encode("a+b/c="), "a%2Bb%2Fc%3D");
        assert_eq!(utc(1_704_205_800).unwrap(), "2024-01-02T14:30:00Z");
        assert!(utc(i64::MAX).is_err());
    }

    #[test]
    fn loading_needs_a_complete_fetch_and_intact_files() {
        let dir = std::env::temp_dir().join(format!("bmv-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = session();
        write_json(&session_path(&dir, &s.date), &s).unwrap();
        let (bars, _) = parse_bars_page(PAGE, &s).unwrap();
        write_json(&bars_path(&dir, "SPY", &s.date), &bars["SPY"]).unwrap();
        write_json(&bars_path(&dir, "QQQ", &s.date), &bars["SPY"]).unwrap();
        let both = syms(&["SPY", "QQQ"]);
        let not_fetched = load_range(&dir, &both, "2024-01-01", "2024-01-31").unwrap_err();
        assert!(
            not_fetched.contains("hasn't been fetched completely"),
            "{not_fetched}"
        );

        record_fetched(
            &dir,
            &Fetched {
                from: "2024-01-01".into(),
                to: "2024-01-31".into(),
                symbols: both.clone(),
            },
        )
        .unwrap();
        let days = load_range(&dir, &both, "2024-01-01", "2024-01-31").unwrap();
        assert_eq!(
            (days.len(), days[0].open, days[0].close, days[0].bars.len()),
            (1, s.open, s.close, 4)
        );
        assert!(
            load_range(&dir, &both, "2024-01-01", "2024-02-15").is_err(),
            "beyond the fetched range"
        );
        assert!(
            load_range(&dir, &syms(&["NVDA"]), "2024-01-01", "2024-01-31").is_err(),
            "symbol not fetched"
        );
        assert!(load_range(&dir, &syms(&["../x"]), "2024-01-01", "2024-01-31").is_err());

        std::fs::write(bars_path(&dir, "QQQ", &s.date), "{oops").unwrap();
        assert!(
            load_range(&dir, &both, "2024-01-01", "2024-01-31")
                .unwrap_err()
                .contains("corrupt")
        );
        assert!(
            read_json::<Vec<Bar>>(&bars_path(&dir, "QQQ", &s.date)).is_err(),
            "so fetch would download it again"
        );
        let leftovers: Vec<_> =
            std::fs::read_dir(bars_path(&dir, "SPY", &s.date).parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                .collect();
        assert!(leftovers.is_empty(), "no temp files left behind");
    }
}
