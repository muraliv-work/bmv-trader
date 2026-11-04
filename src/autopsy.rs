//! Trial autopsies (DESIGN §13d, DECISION S27): where a finished trial won and lost, from its own
//! already-spent trades. Pure functions; the caller supplies the trades and the market regime.

use crate::momentum;
use serde::Serialize;
use std::collections::BTreeMap;

/// One trade (or one month, for monthly trials) as an autopsy sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Entry date, YYYY-MM-DD.
    pub date: String,
    /// After costs, in the trial's units.
    pub net: f64,
    /// "stop", "target", "time", "flatten", "data end" or "month".
    pub exit: &'static str,
    /// Sessions held (None: intraday or monthly).
    pub hold: Option<u32>,
    /// SPY closed above its 200-day average on the signal day (None: unknown).
    pub up_market: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Group {
    pub n: usize,
    pub mean: f64,
    pub win_rate: f64,
}

fn group(rows: &[&Row]) -> Group {
    let net: Vec<f64> = rows.iter().map(|r| r.net).collect();
    Group {
        n: rows.len(),
        mean: momentum::mean(&net),
        win_rate: net.iter().filter(|x| **x > 0.0).count() as f64 / rows.len().max(1) as f64,
    }
}

fn by<K: Ord>(rows: &[Row], key: impl Fn(&Row) -> Option<K>) -> BTreeMap<K, Group> {
    let mut m: BTreeMap<K, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        if let Some(k) = key(r) {
            m.entry(k).or_default().push(r);
        }
    }
    m.into_iter().map(|(k, v)| (k, group(&v))).collect()
}

/// An autopsy: the breakdowns and plain flags.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Autopsy {
    pub n: usize,
    pub net: f64,
    /// The same trial with its costs taken out (None where the trial can't split them).
    pub gross: Option<f64>,
    pub by_year: BTreeMap<String, Group>,
    /// "up" (SPY above its 200-day average on the signal day) and "down".
    pub by_regime: BTreeMap<String, Group>,
    pub by_exit: BTreeMap<String, Group>,
    pub by_hold: BTreeMap<String, Group>,
    pub flags: Vec<String>,
}

/// Hold lengths in buckets, so one-session differences don't scatter the table.
fn hold_bucket(h: u32) -> String {
    match h {
        0..=1 => "1 session".into(),
        2..=3 => "2-3 sessions".into(),
        4..=6 => "4-6 sessions".into(),
        _ => "7+ sessions".into(),
    }
}

pub fn autopsy(rows: &[Row], gross: Option<f64>) -> Autopsy {
    let net = momentum::mean(&rows.iter().map(|r| r.net).collect::<Vec<_>>());
    let by_year = by(rows, |r| r.date.get(..4).map(str::to_string));
    let by_regime = by(rows, |r| {
        r.up_market
            .map(|u| if u { "up" } else { "down" }.to_string())
    });
    let by_exit = by(rows, |r| Some(r.exit.to_string()));
    let by_hold = by(rows, |r| r.hold.map(hold_bucket));
    let mut flags = Vec::new();
    if let Some(g) = gross
        && g > 0.0
        && net < 0.0
    {
        flags.push(format!(
            "costs flipped it: {g:+.3} before costs, {net:+.3} after"
        ));
    }
    if let (Some(u), Some(d)) = (by_regime.get("up"), by_regime.get("down"))
        && (u.mean - d.mean).abs() > 0.2
        && u.n >= 20
        && d.n >= 20
    {
        flags.push(format!(
            "regime-dependent: {:+.3} in up markets, {:+.3} in down",
            u.mean, d.mean
        ));
    }
    // the year contributing the most in total
    if let Some((year, best)) = by_year
        .iter()
        .max_by(|a, b| (a.1.mean * a.1.n as f64).total_cmp(&(b.1.mean * b.1.n as f64)))
    {
        let rest: Vec<f64> = rows
            .iter()
            .filter(|r| !r.date.starts_with(year.as_str()))
            .map(|r| r.net)
            .collect();
        if net > 0.0 && !rest.is_empty() && momentum::mean(&rest) < 0.0 {
            flags.push(format!(
                "one year carried it: without {year} ({:+.3}, {} trades) the mean is {:+.3}",
                best.mean,
                best.n,
                momentum::mean(&rest)
            ));
        }
    }
    if let Some(s) = by_exit.get("stop") {
        let share = s.n as f64 / rows.len().max(1) as f64;
        if share > 0.6 && s.mean < -0.9 {
            flags.push(format!(
                "stops dominate: {:.0}% of exits, averaging {:+.2}",
                share * 100.0,
                s.mean
            ));
        }
    }
    Autopsy {
        n: rows.len(),
        net,
        gross,
        by_year,
        by_regime,
        by_exit,
        by_hold,
        flags,
    }
}

/// Whether SPY closed above its 200-session average, by date, from its daily bars.
pub fn spy_regime(spy: &[crate::scan::DailyBar]) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    let mut sum = 0.0;
    for (i, b) in spy.iter().enumerate() {
        sum += b.close;
        if i >= 200 {
            sum -= spy[i - 200].close;
        }
        if i + 1 >= 200 {
            out.insert(b.date.clone(), b.close > sum / 200.0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(date: &str, net: f64, exit: &'static str, up: bool) -> Row {
        Row {
            date: date.into(),
            net,
            exit,
            hold: Some(3),
            up_market: Some(up),
        }
    }

    fn has(a: &Autopsy, prefix: &str) -> bool {
        a.flags.iter().any(|f| f.starts_with(prefix))
    }

    #[test]
    fn each_flag_fires_only_when_it_should() {
        let costly: Vec<Row> = (0..40)
            .map(|i| row("2020-01-02", -0.05, "time", i % 2 == 0))
            .collect();
        assert!(has(&autopsy(&costly, Some(0.05)), "costs flipped it"));
        assert!(!has(&autopsy(&costly, Some(-0.01)), "costs"));
        assert!(!has(&autopsy(&costly, None), "costs"));

        let mut split: Vec<Row> = (0..30)
            .map(|_| row("2021-03-01", 0.4, "target", true))
            .collect();
        split.extend((0..30).map(|_| row("2021-04-01", -0.2, "stop", false)));
        assert!(has(&autopsy(&split, None), "regime-dependent"));
        let even: Vec<Row> = (0..60)
            .map(|i| row("2021-03-01", 0.1, "target", i % 2 == 0))
            .collect();
        assert!(!has(&autopsy(&even, None), "regime"));

        let mut carried: Vec<Row> = (0..10)
            .map(|_| row("2020-05-01", 2.0, "target", true))
            .collect();
        carried.extend((0..30).map(|_| row("2021-05-01", -0.2, "stop", true)));
        let c = autopsy(&carried, None);
        assert!(c.net > 0.0 && has(&c, "one year carried it: without 2020"));
        let steady: Vec<Row> = ["2020", "2021", "2022"]
            .iter()
            .flat_map(|y| (0..10).map(move |_| row(&format!("{y}-01-02"), 0.2, "target", true)))
            .collect();
        assert!(!has(&autopsy(&steady, None), "one year"));

        let stopped: Vec<Row> = (0..10)
            .map(|i| {
                if i < 7 {
                    row("2022-01-03", -1.05, "stop", true)
                } else {
                    row("2022-01-03", 2.0, "target", true)
                }
            })
            .collect();
        assert!(has(&autopsy(&stopped, None), "stops dominate"));
        let few: Vec<Row> = (0..10)
            .map(|i| {
                row(
                    "2022-01-03",
                    -1.0,
                    if i < 5 { "stop" } else { "time" },
                    true,
                )
            })
            .collect();
        assert!(!has(&autopsy(&few, None), "stops"));
    }

    #[test]
    fn every_finished_trial_has_a_curated_lesson() {
        let text = std::fs::read_to_string("docs/lessons.json").unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        let trials: Vec<u64> = v.iter().map(|x| x["trial"].as_u64().unwrap()).collect();
        assert_eq!(trials, [1, 2, 3, 4, 5, 6, 7, 8, 9, 11]);
        for x in &v {
            assert!(x["lesson"].as_str().is_some_and(|l| l.len() > 20), "{x}");
            assert!(x["acted_on_by"].as_str().is_some(), "{x}");
        }
    }

    #[test]
    fn groups_and_regime() {
        let rows = vec![
            row("2020-01-02", 1.0, "target", true),
            row("2020-06-01", -1.0, "stop", false),
            row("2021-01-04", 0.5, "time", true),
        ];
        let a = autopsy(&rows, None);
        assert_eq!((a.by_year["2020"].n, a.by_year["2021"].n), (2, 1));
        assert_eq!(a.by_regime["up"].mean, 0.75);
        assert_eq!(a.by_exit["stop"].win_rate, 0.0);
        assert_eq!(a.by_hold["2-3 sessions"].n, 3);
        let bars: Vec<crate::scan::DailyBar> = (0..210)
            .map(|i| crate::scan::DailyBar {
                date: format!("d{i:03}"),
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: if i < 205 { 100.0 } else { 90.0 },
                volume: 1,
            })
            .collect();
        let reg = spy_regime(&bars);
        assert_eq!(
            (reg.get("d198"), reg["d199"], reg["d209"]),
            (None, false, false)
        );
        assert_eq!(hold_bucket(9), "7+ sessions");
    }
}
