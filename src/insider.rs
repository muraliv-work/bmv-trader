//! Trial 13's events (DESIGN §7j, DECISION S49): open-market purchases by company insiders, from
//! the SEC's quarterly Insider Transactions Data Sets, and the cluster buys they form.

use crate::scan::DailyBar;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The quarters trial 13 reads: a 10-session window before the 2017-01-03 research start, through
/// the end of the holdout.
pub const FIRST_QUARTER: (i32, u32) = (2016, 4);
pub const LAST_QUARTER: (i32, u32) = (2026, 3);
pub const MIN_OWNERS: usize = 2;
pub const MIN_DOLLARS: f64 = 100_000.0;
pub const WINDOW: usize = 10;
/// A purchase line counts only priced within this band of the unadjusted close (§7j, amended).
pub const PRICE_BAND: (f64, f64) = (0.5, 2.0);
/// Well under the SEC's 10 requests a second.
const SPACING: Duration = Duration::from_secs(2);
const SEC_HOST: &str = "https://www.sec.gov";
/// The SEC moved newer quarters to a second folder; each quarter is tried in both.
const FOLDERS: [&str; 2] = [
    "/files/structureddata/data/insider-transactions-data-sets/",
    "/files/datastandardsinnovation/data/insider-transactions-data-sets/",
];

pub fn quarters() -> Vec<String> {
    let mut out = Vec::new();
    let (mut y, mut q) = FIRST_QUARTER;
    while (y, q) <= LAST_QUARTER {
        out.push(format!("{y}q{q}"));
        (y, q) = if q == 4 { (y + 1, 1) } else { (y, q + 1) };
    }
    out
}

pub fn zip_path(dir: &Path, quarter: &str) -> PathBuf {
    dir.join("insider").join(format!("{quarter}_form345.zip"))
}

/// The contact the SEC requires in the User-Agent, from the gitignored `data/sec_contact`.
pub fn contact(dir: &Path) -> Result<String, String> {
    let path = dir.join("sec_contact");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e}; write your contact email there (the SEC requires one)",
            path.display()
        )
    })?;
    valid_contact(text.trim())
        .map(str::to_string)
        .ok_or_else(|| format!("{} must hold one email address", path.display()))
}

/// One plain address: it goes into an HTTP header, so nothing that could split or fold it.
fn valid_contact(s: &str) -> Option<&str> {
    let ok = s.len() <= 254
        && s.split_once('@')
            .is_some_and(|(a, d)| !a.is_empty() && d.contains('.'))
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@.+-_".contains(c));
    ok.then_some(s)
}

/// Downloads every quarter not already in `data/insider/`. Returns how many it fetched.
pub fn fetch(dir: &Path, contact: &str) -> Result<usize, String> {
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(10))
        .no_proxy()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("bmv-trader {contact}"))
        .build()
        .map_err(|e| format!("SEC client: {e}"))?;
    std::fs::create_dir_all(dir.join("insider")).map_err(|e| format!("data/insider: {e}"))?;
    let mut fetched = 0;
    for q in quarters() {
        let path = zip_path(dir, &q);
        if path.exists() {
            continue;
        }
        let mut last = String::new();
        let mut got = None;
        for folder in FOLDERS {
            std::thread::sleep(SPACING);
            let url = format!("{SEC_HOST}{folder}{q}_form345.zip");
            let resp = http.get(&url).send().map_err(|e| format!("{q}: {e}"))?;
            if resp.status().is_success() {
                got = Some(resp.bytes().map_err(|e| format!("{q}: {e}"))?);
                break;
            }
            last = format!("{url}: HTTP {}", resp.status());
        }
        let bytes = got.ok_or(last)?;
        if !bytes.starts_with(b"PK") {
            return Err(format!("{q}: the SEC sent something that isn't a ZIP"));
        }
        let tmp = path.with_extension("zip.part");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("  {q}: {} MB", bytes.len() / 1_000_000);
        fetched += 1;
    }
    Ok(fetched)
}

/// One Form 4's qualifying purchases (§7j): code `P`, acquired, shares and price above 0.
#[derive(Clone, Debug, PartialEq)]
pub struct Purchase {
    pub accession: String,
    pub ticker: String,
    /// The filing date, YYYY-MM-DD.
    pub filed: String,
    /// The filing's first reporting owner; a joint filing counts once.
    pub owner: String,
    /// Each qualifying line's (shares, price per share).
    pub lines: Vec<(f64, f64)>,
    /// Shares × price, summed over the lines.
    pub dollars: f64,
}

/// `2017-02-03` → `2017q1`.
pub fn quarter(filed: &str) -> String {
    let month: u32 = filed.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(1);
    format!("{}q{}", filed.get(..4).unwrap_or("?"), month.div_ceil(3))
}

/// The purchase with only its lines priced near the market (§7j, amended): within
/// `PRICE_BAND` of the unadjusted close on the last session on or before the filing date.
/// None when no such bar exists or no line is left.
pub fn near_market(p: &Purchase, raw: &[DailyBar]) -> Option<Purchase> {
    let at = raw.partition_point(|b| b.date.as_str() <= p.filed.as_str());
    let close = raw.get(at.checked_sub(1)?)?.close;
    if !(close.is_finite() && close > 0.0) {
        return None;
    }
    let lines: Vec<(f64, f64)> = p
        .lines
        .iter()
        .copied()
        .filter(|(_, price)| (PRICE_BAND.0..=PRICE_BAND.1).contains(&(price / close)))
        .collect();
    (!lines.is_empty()).then(|| Purchase {
        dollars: lines.iter().map(|(n, x)| n * x).sum(),
        lines,
        ..p.clone()
    })
}

/// `31-OCT-2016` → `2016-10-31`.
fn iso_date(s: &str) -> Option<String> {
    let mut p = s.trim().split('-');
    let (d, m, y) = (p.next()?, p.next()?, p.next()?);
    let months = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let m = months.iter().position(|x| x.eq_ignore_ascii_case(m))? + 1;
    let (d, y): (u32, i32) = (d.parse().ok()?, y.parse().ok()?);
    chrono::NaiveDate::from_ymd_opt(y, m as u32, d).map(|d| d.to_string())
}

/// A tab-separated table's rows as column-name lookups.
fn rows(text: &str) -> Result<Vec<BTreeMap<&str, &str>>, String> {
    let mut lines = text.lines();
    let head: Vec<&str> = lines.next().ok_or("an empty table")?.split('\t').collect();
    Ok(lines
        .filter(|l| !l.is_empty())
        .map(|l| head.iter().copied().zip(l.split('\t')).collect())
        .collect())
}

fn col<'a>(row: &BTreeMap<&str, &'a str>, name: &str) -> Result<&'a str, String> {
    row.get(name).copied().ok_or(format!("no {name} column"))
}

/// The purchases in one quarter's SUBMISSION, REPORTINGOWNER and NONDERIV_TRANS tables.
pub fn parse(submission: &str, owners: &str, trans: &str) -> Result<Vec<Purchase>, String> {
    let mut filings: BTreeMap<&str, (String, String)> = BTreeMap::new();
    for r in rows(submission)? {
        if col(&r, "DOCUMENT_TYPE")?.trim() != "4" {
            continue;
        }
        let ticker = col(&r, "ISSUERTRADINGSYMBOL")?.trim().to_ascii_uppercase();
        if let (false, Some(filed)) = (ticker.is_empty(), iso_date(col(&r, "FILING_DATE")?)) {
            filings.insert(col(&r, "ACCESSION_NUMBER")?, (ticker, filed));
        }
    }
    let mut first_owner: BTreeMap<&str, &str> = BTreeMap::new();
    for r in rows(owners)? {
        first_owner
            .entry(col(&r, "ACCESSION_NUMBER")?)
            .or_insert(col(&r, "RPTOWNERCIK")?);
    }
    let mut lines: BTreeMap<&str, Vec<(f64, f64)>> = BTreeMap::new();
    for r in rows(trans)? {
        let num = |c: &str| col(&r, c).map(|v| v.trim().parse::<f64>().unwrap_or(0.0));
        let (shares, price) = (num("TRANS_SHARES")?, num("TRANS_PRICEPERSHARE")?);
        if col(&r, "TRANS_CODE")?.trim() == "P"
            && col(&r, "TRANS_ACQUIRED_DISP_CD")?.trim() == "A"
            && shares > 0.0
            && price > 0.0
            && shares.is_finite()
            && price.is_finite()
        {
            lines
                .entry(col(&r, "ACCESSION_NUMBER")?)
                .or_default()
                .push((shares, price));
        }
    }
    Ok(lines
        .into_iter()
        .filter_map(|(acc, lines)| {
            let (ticker, filed) = filings.get(acc)?;
            Some(Purchase {
                accession: acc.to_string(),
                ticker: ticker.clone(),
                filed: filed.clone(),
                owner: first_owner.get(acc)?.to_string(),
                dollars: lines.iter().map(|(n, x)| n * x).sum(),
                lines,
            })
        })
        .collect())
}

fn table(zip: &Path, name: &str) -> Result<String, String> {
    let out = std::process::Command::new("/usr/bin/unzip")
        .arg("-p")
        .arg(zip)
        .arg(name)
        .output()
        .map_err(|e| format!("unzip {}: {e}", zip.display()))?;
    if !out.status.success() {
        return Err(format!("{}: no {name}", zip.display()));
    }
    // names are sometimes Latin-1; only the ASCII columns matter
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// How many new purchases each quarter added.
pub type PerQuarter = Vec<(String, usize)>;

/// Every quarter's purchases, each filing once, with the count per quarter.
pub fn purchases(dir: &Path) -> Result<(Vec<Purchase>, PerQuarter), String> {
    let (mut all, mut per_quarter) = (Vec::new(), Vec::new());
    let mut seen = std::collections::BTreeSet::new();
    for q in quarters() {
        let zip = zip_path(dir, &q);
        if !zip.exists() {
            return Err(format!(
                "{} is missing: run `bmv-trader insider-fetch`",
                zip.display()
            ));
        }
        let got = parse(
            &table(&zip, "SUBMISSION.tsv")?,
            &table(&zip, "REPORTINGOWNER.tsv")?,
            &table(&zip, "NONDERIV_TRANS.tsv")?,
        )
        .map_err(|e| format!("{q}: {e}"))?;
        let before = all.len();
        all.extend(got.into_iter().filter(|p| seen.insert(p.accession.clone())));
        per_quarter.push((q, all.len() - before));
    }
    Ok((all, per_quarter))
}

/// Cluster signals (§7j) by ticker: session → the window's dollars, on each session where 2+
/// distinct owners total $100k+ over the last 10 sessions and didn't the session before. A
/// filing's session is the first on or after its filing date.
pub fn clusters(buys: &[Purchase], sessions: &[String]) -> BTreeMap<String, BTreeMap<usize, f64>> {
    let mut by_ticker: BTreeMap<&str, Vec<(usize, &str, f64)>> = BTreeMap::new();
    for p in buys {
        let s = sessions.partition_point(|d| d.as_str() < p.filed.as_str());
        if s < sessions.len() {
            by_ticker
                .entry(&p.ticker)
                .or_default()
                .push((s, &p.owner, p.dollars));
        }
    }
    let mut out = BTreeMap::new();
    for (ticker, buys) in by_ticker {
        // the window over sessions (end - 10, end]
        let holds = |end: usize| {
            let inside = buys.iter().filter(|b| b.0 <= end && b.0 + WINDOW > end);
            let owners: std::collections::BTreeSet<&str> = inside.clone().map(|b| b.1).collect();
            let total: f64 = inside.map(|b| b.2).sum();
            (owners.len() >= MIN_OWNERS && total >= MIN_DOLLARS).then_some(total)
        };
        let days: std::collections::BTreeSet<usize> = buys.iter().map(|b| b.0).collect();
        let fired: BTreeMap<usize, f64> = days
            .into_iter()
            .filter_map(|s| {
                let now = holds(s)?;
                (s == 0 || holds(s - 1).is_none()).then_some((s, now))
            })
            .collect();
        if !fired.is_empty() {
            out.insert(ticker.to_string(), fired);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUB: &str = "ACCESSION_NUMBER\tFILING_DATE\tDOCUMENT_TYPE\tISSUERTRADINGSYMBOL\n\
        a1\t03-JAN-2017\t4\tabc \n\
        a2\t04-JAN-2017\t4/A\tABC\n\
        a3\t05-JAN-2017\t4\tABC\n\
        a4\t05-JAN-2017\t4\t\n";
    const OWN: &str = "ACCESSION_NUMBER\tRPTOWNERCIK\na1\tX\na1\tX\na1\tY\na2\tZ\na3\tY\na4\tW\n";
    const TRANS: &str = "ACCESSION_NUMBER\tTRANS_CODE\tTRANS_SHARES\tTRANS_PRICEPERSHARE\tTRANS_ACQUIRED_DISP_CD\n\
        a1\tP\t1000\t50.0\tA\n\
        a1\tP\t100\t10\tA\n\
        a1\tS\t5000\t50\tD\n\
        a1\tP\t100\t0\tA\n\
        a2\tP\t9000\t50\tA\n\
        a3\tP\t2000\t25\tA\n\
        a4\tP\t9000\t50\tA\n";

    #[test]
    fn only_original_form_4_purchases_with_a_ticker_and_a_price_count() {
        let p = parse(SUB, OWN, TRANS).unwrap();
        let got: Vec<(&str, &str, &str, &str, f64)> = p
            .iter()
            .map(|p| {
                (
                    p.accession.as_str(),
                    p.ticker.as_str(),
                    p.filed.as_str(),
                    p.owner.as_str(),
                    p.dollars,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a1", "ABC", "2017-01-03", "X", 51_000.0),
                ("a3", "ABC", "2017-01-05", "Y", 50_000.0)
            ]
        );
    }

    #[test]
    fn a_missing_column_is_an_error_not_an_empty_quarter() {
        assert!(parse("ACCESSION_NUMBER\tFILING_DATE\nx\ty\n", OWN, TRANS).is_err());
    }

    #[test]
    fn dates_convert_and_bad_ones_drop() {
        assert_eq!(iso_date("31-OCT-2016").as_deref(), Some("2016-10-31"));
        assert_eq!(iso_date("31-FOO-2016"), None);
        assert_eq!(iso_date("31-FEB-2016"), None);
    }

    fn buy(owner: &str, filed: &str, dollars: f64) -> Purchase {
        Purchase {
            accession: format!("{owner}{filed}"),
            ticker: "ABC".into(),
            filed: filed.into(),
            owner: owner.into(),
            lines: vec![(dollars / 10.0, 10.0)],
            dollars,
        }
    }

    fn close_on(date: &str, close: f64) -> DailyBar {
        DailyBar {
            date: date.into(),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1,
        }
    }

    #[test]
    fn only_lines_priced_near_the_last_close_on_or_before_filing_count() {
        let raw = [close_on("2017-01-03", 10.0), close_on("2017-01-05", 40.0)];
        let mut p = buy("X", "2017-01-04", 0.0);
        p.lines = vec![
            (100.0, 5.0),
            (100.0, 20.0),
            (100.0, 4.99),
            (100.0, 20.01),
            (3.6e6, 2.7e6),
        ];
        let kept = near_market(&p, &raw).unwrap();
        assert_eq!(
            kept.lines,
            [(100.0, 5.0), (100.0, 20.0)],
            "the 01-03 close, not the later 01-05"
        );
        assert_eq!(kept.dollars, 2_500.0);
        p.lines = vec![(1.0, 1_000.0)];
        assert_eq!(near_market(&p, &raw), None, "nothing left");
        p.filed = "2017-01-02".into();
        p.lines = vec![(1.0, 10.0)];
        assert_eq!(
            near_market(&p, &raw),
            None,
            "no bar on or before the filing"
        );
    }

    #[test]
    fn quarters_come_from_the_filing_month() {
        assert_eq!(quarter("2017-03-31"), "2017q1");
        assert_eq!(quarter("2017-04-01"), "2017q2");
        assert_eq!(quarter("2026-12-01"), "2026q4");
    }

    fn sessions(n: usize) -> Vec<String> {
        (1..=n).map(|d| format!("2017-01-{d:02}")).collect()
    }

    #[test]
    fn a_cluster_needs_two_owners_and_100k_within_ten_sessions() {
        let cal = sessions(30);
        let fired = |b: &[Purchase]| clusters(b, &cal).get("ABC").cloned().unwrap_or_default();
        // exactly at the bar: fires on the session of the second filing, not the first
        let at = fired(&[
            buy("X", "2017-01-02", 60_000.0),
            buy("Y", "2017-01-05", 40_000.0),
        ]);
        assert_eq!(at.into_iter().collect::<Vec<_>>(), [(4, 100_000.0)]);
        assert!(
            fired(&[
                buy("X", "2017-01-02", 60_000.0),
                buy("Y", "2017-01-05", 39_999.0)
            ])
            .is_empty()
        );
        assert!(
            fired(&[
                buy("X", "2017-01-02", 90_000.0),
                buy("X", "2017-01-05", 90_000.0)
            ])
            .is_empty()
        );
        // ten sessions apart is outside the window; nine is inside
        assert!(
            fired(&[
                buy("X", "2017-01-01", 60_000.0),
                buy("Y", "2017-01-11", 60_000.0)
            ])
            .is_empty()
        );
        assert_eq!(
            fired(&[
                buy("X", "2017-01-01", 60_000.0),
                buy("Y", "2017-01-10", 60_000.0)
            ])
            .len(),
            1
        );
    }

    #[test]
    fn a_cluster_fires_once_and_again_only_after_it_lapses() {
        let cal = sessions(30);
        let b = [
            buy("X", "2017-01-01", 60_000.0),
            buy("Y", "2017-01-02", 60_000.0),
            buy("Z", "2017-01-03", 60_000.0),
            buy("X", "2017-01-20", 60_000.0),
            buy("Y", "2017-01-21", 60_000.0),
        ];
        let got: Vec<usize> = clusters(&b, &cal)["ABC"].keys().copied().collect();
        assert_eq!(got, [1, 20]);
    }

    #[test]
    fn a_filing_counts_from_the_first_session_on_or_after_its_date() {
        let cal = vec!["2017-01-03".to_string(), "2017-01-05".to_string()];
        let b = [
            buy("X", "2017-01-03", 60_000.0),
            buy("Y", "2017-01-04", 60_000.0),
        ];
        assert_eq!(
            clusters(&b, &cal)["ABC"]
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [1]
        );
        let late = [
            buy("X", "2017-01-06", 60_000.0),
            buy("Y", "2017-01-06", 60_000.0),
        ];
        assert!(clusters(&late, &cal).is_empty(), "after the last session");
    }

    #[test]
    fn the_quarters_run_from_2016q4_through_2026q3() {
        let q = quarters();
        assert_eq!(q.first().unwrap(), "2016q4");
        assert_eq!(q.last().unwrap(), "2026q3");
        assert_eq!(q.len(), 40);
    }

    #[test]
    fn a_contact_is_one_plain_email() {
        assert_eq!(
            valid_contact("a.b+sec@gmail.com"),
            Some("a.b+sec@gmail.com")
        );
        for bad in [
            "",
            "nobody",
            "a@b",
            "a@b.com\r\nX: y",
            "a b@c.com",
            "@c.com",
        ] {
            assert_eq!(valid_contact(bad), None, "{bad:?}");
        }
    }
}
