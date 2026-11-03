//! Historical 1-minute bars from Alpaca for research (DESIGN §13), cached under `data/history/`.
//! The feed is always explicit (`iex`, the same feed the live engine trades on) and part of the
//! cache path, so backtests never silently mix feeds. Sessions come from Alpaca's calendar, so
//! half-days have their real close. Downloads resume: a cached (symbol, day) is never fetched again.
//!
//! Layout: `history/sessions/<date>.json` and `history/<feed>/<SYMBOL>/<date>.json`.

use crate::backtest::Day;
use crate::broker::{Alpaca, Api};
use crate::strategy::Bar;
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::America::New_York;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FEED: &str = "iex";

type BarsBySymbol = BTreeMap<String, Vec<Bar>>;

/// Free plan: 200 historical calls a minute. One call every 350 ms stays under it.
pub const CALL_SPACING_MS: u64 = 350;

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
            let day = NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
                .map_err(|e| format!("calendar date {:?}: {e}", r.date))?;
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

/// Parses one page of `/v2/stocks/bars`. Bars outside the session are dropped.
fn parse_bars_page(json: &str, s: &Session) -> Result<(BarsBySymbol, Option<String>), String> {
    let page: BarsPage = serde_json::from_str(json).map_err(|e| format!("bars page: {e}"))?;
    let mut out: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
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

fn utc(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// Percent-encodes everything except unreserved URL characters.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Downloads one session's bars for `symbols`, following pagination. `wait` runs before each call.
fn fetch_session(
    alpaca: &Alpaca,
    symbols: &[String],
    s: &Session,
    wait: &mut dyn FnMut(),
) -> Result<BarsBySymbol, String> {
    let mut all: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
    let mut token: Option<String> = None;
    loop {
        let mut path = format!(
            "/v2/stocks/bars?symbols={}&timeframe=1Min&start={}&end={}&feed={FEED}&adjustment=raw&limit=10000",
            encode(&symbols.join(",")),
            encode(&utc(s.open)),
            encode(&utc(s.close)),
        );
        if let Some(t) = &token {
            path.push_str(&format!("&page_token={}", encode(t)));
        }
        wait();
        let raw: serde_json::Value = alpaca.get(Api::MarketData, &path)?;
        let (bars, next) = parse_bars_page(&raw.to_string(), s)?;
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

/// Writes via a temp file and rename, so an interrupted download never leaves a half file.
fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename into {}: {e}", path.display()))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("corrupt cache file {}: {e}", path.display()))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchReport {
    pub sessions: usize,
    pub downloaded: usize,
    pub cached: usize,
}

/// Downloads every session in [start, end] (YYYY-MM-DD) for `symbols` into `dir`, skipping what is
/// already cached. A day with no bars for a symbol is cached as empty, so it isn't fetched again.
pub fn fetch_range(
    alpaca: &Alpaca,
    dir: &Path,
    symbols: &[String],
    start: &str,
    end: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<FetchReport, String> {
    let rows: Vec<CalendarRow> = alpaca.get(
        Api::Trading,
        &format!("/v2/calendar?start={start}&end={end}"),
    )?;
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
            .filter(|sym| !bars_path(dir, sym, &s.date).exists())
            .cloned()
            .collect();
        report.cached += symbols.len() - missing.len();
        if missing.is_empty() {
            continue;
        }
        let mut got = fetch_session(alpaca, &missing, s, &mut wait)?;
        for sym in &missing {
            write_json(
                &bars_path(dir, sym, &s.date),
                &got.remove(sym).unwrap_or_default(),
            )?;
        }
        report.downloaded += missing.len();
        progress(&s.date);
    }
    Ok(report)
}

/// Loads cached sessions in [start, end] for `symbols`. Anything not downloaded is an error,
/// so a backtest never runs on silently missing data.
pub fn load_range(
    dir: &Path,
    symbols: &[String],
    start: &str,
    end: &str,
) -> Result<Vec<Day>, String> {
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
            let mut bars = Vec::new();
            for sym in symbols {
                let p = bars_path(dir, sym, date);
                if !p.exists() {
                    return Err(format!("{sym} {date} not downloaded; run fetch first"));
                }
                let b: Vec<Bar> = read_json(&p)?;
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
        let bad = |open: &str, close: &str| {
            parse_calendar(&[CalendarRow {
                date: "2024-01-02".into(),
                open: open.into(),
                close: close.into(),
            }])
            .is_err()
        };
        assert!(bad("9h30", "16:00"));
        assert!(bad("16:00", "09:30"));
        assert!(
            parse_calendar(&[CalendarRow {
                date: "Jan 2".into(),
                open: "09:30".into(),
                close: "16:00".into()
            }])
            .is_err()
        );
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
    fn urls_are_encoded() {
        assert_eq!(encode("SPY,BRK.B"), "SPY%2CBRK.B");
        assert_eq!(encode("2024-01-02T14:30:00Z"), "2024-01-02T14%3A30%3A00Z");
        assert_eq!(encode("a+b/c="), "a%2Bb%2Fc%3D");
        assert_eq!(utc(1_704_205_800), "2024-01-02T14:30:00Z");
    }

    #[test]
    fn cache_round_trip_and_missing_data_is_an_error() {
        let dir = std::env::temp_dir().join(format!("bmv-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = session();
        write_json(&session_path(&dir, &s.date), &s).unwrap();
        let (bars, _) = parse_bars_page(PAGE, &s).unwrap();
        write_json(&bars_path(&dir, "SPY", &s.date), &bars["SPY"]).unwrap();
        write_json(&bars_path(&dir, "QQQ", &s.date), &Vec::<Bar>::new()).unwrap();
        let syms = vec!["SPY".to_string(), "QQQ".to_string()];
        let days = load_range(&dir, &syms, "2024-01-01", "2024-01-31").unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(
            (days[0].open, days[0].close, days[0].bars.len()),
            (s.open, s.close, 2)
        );
        assert!(
            load_range(&dir, &syms, "2025-01-01", "2025-12-31")
                .unwrap()
                .is_empty()
        );
        let err = load_range(&dir, &["NVDA".to_string()], "2024-01-01", "2024-01-31").unwrap_err();
        assert!(err.contains("not downloaded"), "{err}");
        std::fs::write(bars_path(&dir, "QQQ", &s.date), "{oops").unwrap();
        assert!(
            load_range(&dir, &syms, "2024-01-01", "2024-01-31")
                .unwrap_err()
                .contains("corrupt")
        );
        assert!(load_range(&dir.join("nope"), &syms, "a", "z").is_err());
    }
}
