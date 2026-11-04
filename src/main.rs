use bmv_trader::backtest::{self, DayResult, FillModel};
use bmv_trader::engine::{self, Entry, Settings};
use bmv_trader::strategy::OrbParams;
use bmv_trader::{broker, config, config::Mode, history, live, stats, ui, watch};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::exit;

const USAGE: &str = "usage:
  bmv-trader account
  bmv-trader status                       account, positions and open orders (read-only)
  bmv-trader watch                        live read-only monitor, refreshes every 5 s (Ctrl-C quits)
  bmv-trader ui       [--port 8765]       web console on 127.0.0.1: the watch view plus STOP and flatten
  bmv-trader run      [--symbols ...] [--trend] [--marketable]   one paper session (PAPER ONLY; live is P5)
  bmv-trader flatten                      close every position and confirm (any mode)
  bmv-trader backstop                     for launchd: near the close, flatten anything left (deploy/)
  bmv-trader fetch    --from YYYY-MM-DD --to YYYY-MM-DD [--symbols SPY,QQQ,...] [--allow-gaps]
  bmv-trader universe                     list every US stock, active and delisted (for the scan)
  bmv-trader fetch-daily --from YYYY-MM-DD --to YYYY-MM-DD [--adjusted]   daily bars for the universe
                      (raw IEX for the scan; --adjusted: split- and dividend-adjusted SIP, for momentum)
  bmv-trader momentum                     trial 7: 12-1 month momentum on the adjusted daily bars (DESIGN §13c)
  bmv-trader asset-names                  Alpaca's name for each universe symbol (for trial 9's fund filter)
  bmv-trader backtest-swing --trial 8|9   the Desk Signal swing strategy on the adjusted daily bars (DESIGN §7d, §7e)
  bmv-trader scan     --from YYYY-MM-DD --to YYYY-MM-DD   each day's gappers, and their minute bars
  bmv-trader backtest --from YYYY-MM-DD --to YYYY-MM-DD [--strategy orb|gap|vwap] [--scan] [--symbols ...] [--fold-days 21]
                      [--seeds 5] [--holdout] [--holdout-again] [--trend] [--marketable]

The holdout start date is pinned in data/holdout_from on the first backtest (the last 63 sessions
loaded) and research never uses days from it on. --holdout evaluates it once; a second evaluation
of the same holdout needs --holdout-again, and should be rare. --trend only takes breakouts while
SPY and the symbol trend up today (above VWAP and the open); it is logged as its own trial.";

/// The v1 watchlist (DESIGN §7).
const WATCHLIST: &str = "SPY,QQQ,AAPL,MSFT,NVDA,AMD,META,AMZN,GOOGL,TSLA";
/// About 3 months (DESIGN §13).
const HOLDOUT_SESSIONS: usize = 63;
const MAX_SEEDS: usize = 50;

/// Flags taking a value, and switches. Anything else is an error, so a typo can't silently
/// fall back to a default.
const VALUE_FLAGS: [&str; 8] = [
    "--trial",
    "--strategy",
    "--from",
    "--to",
    "--symbols",
    "--fold-days",
    "--seeds",
    "--port",
];
const SWITCHES: [&str; 7] = [
    "--adjusted",
    "--scan",
    "--holdout",
    "--holdout-again",
    "--trend",
    "--allow-gaps",
    "--marketable",
];

struct Args {
    values: BTreeMap<String, String>,
    switches: Vec<String>,
}

impl Args {
    fn parse(rest: &[String]) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        let mut switches = Vec::new();
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            if VALUE_FLAGS.contains(&a.as_str()) {
                match it.next() {
                    Some(v) if !v.starts_with("--") => {
                        values.insert(a.clone(), v.clone());
                    }
                    _ => return Err(format!("{a} needs a value")),
                }
            } else if SWITCHES.contains(&a.as_str()) {
                switches.push(a.clone());
            } else {
                return Err(format!("unknown argument {a:?}\n{USAGE}"));
            }
        }
        Ok(Args { values, switches })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn num(
        &self,
        name: &str,
        default: usize,
        range: std::ops::RangeInclusive<usize>,
    ) -> Result<usize, String> {
        let n = match self.get(name) {
            None => default,
            Some(v) => v
                .parse()
                .map_err(|_| format!("{name} must be a whole number, got {v:?}"))?,
        };
        if range.contains(&n) {
            Ok(n)
        } else {
            Err(format!(
                "{name} must be between {} and {}",
                range.start(),
                range.end()
            ))
        }
    }

    fn dates(&self) -> Result<(String, String), String> {
        match (self.get("--from"), self.get("--to")) {
            (Some(f), Some(t)) => {
                if history::canonical_date(f)? > history::canonical_date(t)? {
                    return Err(format!("--from {f} is after --to {t}"));
                }
                Ok((f.to_string(), t.to_string()))
            }
            _ => Err(format!("need --from and --to\n{USAGE}")),
        }
    }

    fn symbols(&self) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        for s in self.get("--symbols").unwrap_or(WATCHLIST).split(',') {
            let s = s.trim().to_ascii_uppercase();
            if !history::valid_symbol(&s) {
                return Err(format!("invalid symbol {s:?}"));
            }
            if !out.contains(&s) {
                out.push(s);
            }
        }
        Ok(out)
    }
}

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let result = Args::parse(all.get(1..).unwrap_or_default()).and_then(|args| {
        match all.first().map(String::as_str) {
            Some("account") => account(),
            Some("status") => status(),
            Some("watch") => watch(),
            Some("ui") => ui(args.num("--port", 8765, 1024..=65535)?),
            Some("flatten") => flatten(),
            Some("backstop") => backstop(),
            Some("run") => run(&args.symbols()?, args.has("--trend"), orb_params(&args)),
            Some("fetch") => {
                let (from, to) = args.dates()?;
                fetch(&from, &to, &args.symbols()?, args.has("--allow-gaps"))
            }
            Some("universe") => universe(),
            Some("fetch-daily") => {
                let (from, to) = args.dates()?;
                fetch_daily(&from, &to, args.has("--adjusted"))
            }
            Some("momentum") => momentum(),
            Some("asset-names") => {
                let n = bmv_trader::scan::fetch_asset_names(&connect()?, &data_dir()?)?;
                println!("names for {n} symbols");
                Ok(())
            }
            Some("backtest-swing") => backtest_swing(match args.get("--trial") {
                Some("8") => bmv_trader::swing::TRIAL_8,
                Some("9") => bmv_trader::swing::TRIAL_9,
                other => return Err(format!("--trial must be 8 or 9, got {other:?}")),
            }),
            Some("scan") => {
                let (from, to) = args.dates()?;
                scan(&from, &to)
            }
            Some("backtest") => {
                let (from, to) = args.dates()?;
                let opts = Opts {
                    fold_days: args.num("--fold-days", 21, 5..=126)?,
                    seeds: args.num("--seeds", 5, 1..=MAX_SEEDS)?,
                    holdout: args.has("--holdout"),
                    holdout_again: args.has("--holdout-again"),
                    trend: args.has("--trend"),
                    scan: args.has("--scan"),
                    symbols_given: args.get("--symbols").is_some(),
                    orb: orb_params(&args),
                    strategy: match args.get("--strategy").unwrap_or("orb") {
                        "orb" => Strategy::Orb,
                        "gap" => Strategy::Gap,
                        "vwap" => Strategy::Vwap,
                        other => {
                            return Err(format!(
                                "--strategy must be orb, gap or vwap, got {other:?}"
                            ));
                        }
                    },
                };
                run_backtest(&from, &to, &args.symbols()?, &opts)
            }
            _ => Err(USAGE.to_string()),
        }
    });
    if let Err(e) = result {
        fail(&e);
    }
}

fn connect() -> Result<broker::Alpaca, String> {
    let cfg = config::load()?;
    match cfg.mode() {
        Mode::Paper => eprintln!("mode: PAPER"),
        Mode::Live => eprintln!("\x1b[1;41m  LIVE TRADING — REAL MONEY  \x1b[0m"),
    }
    broker::Alpaca::new(cfg)
}

fn account() -> Result<(), String> {
    print_account(&connect()?)
}

fn print_account(alpaca: &broker::Alpaca) -> Result<(), String> {
    let a = alpaca.account()?;
    for (k, v) in [
        ("status", a.status),
        ("currency", a.currency),
        ("cash", a.cash.to_string()),
        ("buying_power", a.buying_power.to_string()),
        ("equity", a.equity.to_string()),
        ("today_pnl", (a.equity - a.last_equity).to_string()),
        ("trading_blocked", a.trading_blocked.to_string()),
        ("account_blocked", a.account_blocked.to_string()),
    ] {
        println!("{k:>16}: {v}");
    }
    Ok(())
}

/// Read-only: account, positions, open orders.
fn status() -> Result<(), String> {
    let a = connect()?;
    print_account(&a)?;
    println!("positions:");
    for p in a.positions()? {
        println!("  {:>6} {:>6} @ {}", p.symbol, p.qty, p.avg_entry_price);
    }
    println!("open orders:");
    for o in live::open_orders(&a)? {
        println!(
            "  {} {} {} {} {}/{}",
            o.client_order_id, o.symbol, o.side, o.status, o.filled_qty, o.qty
        );
    }
    Ok(())
}

fn mode_name() -> Result<String, String> {
    Ok(format!("{:?}", config::load()?.mode()).to_uppercase())
}

/// Read-only monitor. Alpaca errors are shown and retried next refresh, never fatal.
fn watch() -> Result<(), String> {
    let mode = mode_name()?;
    let (a, home) = (connect()?, config::home()?);
    loop {
        let s = watch::snapshot(&a, &home, &mode, 12);
        let mut out = String::from("\x1b[2J\x1b[H"); // clear screen, cursor home
        out += &format!(
            "bmv-trader watch · {} ET · {mode} · read-only (Ctrl-C quits)\n\n",
            s.time_et
        );
        out += &format!(
            "  state: {}   session: {}   STOP file: {}\n",
            s.state,
            s.summary.session.as_deref().unwrap_or("-"),
            s.stop
                .as_deref()
                .map_or("clear".into(), |w| format!("ENGAGED ({w})"))
        );
        if let Some(why) = &s.summary.halted {
            out += &format!("  halt reason: {why}\n");
        }
        if !s.summary.unresolved.is_empty() {
            let keys: Vec<&str> = s.summary.unresolved.iter().map(String::as_str).collect();
            out += &format!("  unresolved: {}\n", keys.join(", "));
        }
        if s.summary.unreadable > 0 {
            out += &format!(
                "  WARNING: {} unreadable journal lines\n",
                s.summary.unreadable
            );
        }
        for (what, e) in &s.errors {
            out += &format!("  {what}: ERROR {e}\n");
        }
        let c = &s.summary;
        out += &format!(
            "  today: {} candidates · {} rejected · {} orders · {} fills · {} exits\n\n",
            c.candidates, c.rejections, c.orders, c.fills, c.exits
        );
        if let Some(acct) = &s.account {
            out += &format!(
                "  equity {}   today P&L {}   buying power {}\n",
                acct.equity, acct.today_pnl, acct.buying_power
            );
        }
        match &s.positions {
            Some(ps) if ps.is_empty() => out += "  positions: none\n",
            Some(ps) => {
                out += "  positions:\n";
                for p in ps {
                    out += &format!("    {:>6} {:>6} @ {}\n", p.symbol, p.qty, p.avg_entry_price);
                }
            }
            None => {}
        }
        match &s.orders {
            Some(os) if os.is_empty() => out += "  open orders: none\n",
            Some(os) => {
                out += "  open orders:\n";
                for o in os {
                    out += &format!(
                        "    {} {} {} {} {}/{}\n",
                        o.client_order_id, o.symbol, o.side, o.status, o.filled_qty, o.qty
                    );
                }
            }
            None => {}
        }
        out += "\n  recent journal:\n";
        for l in &s.summary.recent {
            let detail: String = l.detail.chars().take(70).collect();
            out += &format!(
                "    {} {:<12} {:<22} {detail}\n",
                watch::et(l.ts),
                l.kind,
                l.id
            );
        }
        print!("{out}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// Web console on 127.0.0.1 (DECISION S16); serves until Ctrl-C.
fn ui(port: usize) -> Result<(), String> {
    let mode = mode_name()?;
    let port = u16::try_from(port).map_err(|_| "--port out of range".to_string())?;
    ui::serve(&connect()?, &config::home()?, &mode, port)
}

/// launchd runs this every 2 minutes (deploy/). Between closes it exits at once, without the
/// network or the Keychain; near a close it flattens anything left (any mode, like `flatten`).
fn backstop() -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    if !live::maybe_near_close(now) {
        return Ok(());
    }
    let msg = live::backstop(&connect()?, now)?;
    println!("{} {msg}", chrono::Utc::now().to_rfc3339());
    Ok(())
}

/// The emergency exit, allowed in both modes.
fn flatten() -> Result<(), String> {
    let a = connect()?;
    live::flatten(&a, 10, &mut || {
        std::thread::sleep(std::time::Duration::from_secs(1))
    })?;
    println!("flat");
    Ok(())
}

/// The scan universe: every listed US stock, active and delisted (DESIGN §13b).
fn universe() -> Result<(), String> {
    let n = bmv_trader::scan::fetch_universe(&connect()?, &data_dir()?)?;
    println!("{n} symbols in data/daily/universe.json");
    Ok(())
}

fn fetch_daily(from: &str, to: &str, adjusted: bool) -> Result<(), String> {
    use bmv_trader::scan::{ADJUSTED_SIP, RAW_IEX};
    let set = if adjusted { ADJUSTED_SIP } else { RAW_IEX };
    let n =
        bmv_trader::scan::fetch_daily(&connect()?, &data_dir()?, set, from, to, &mut |d, n| {
            eprintln!("  {d}/{n} symbols")
        })?;
    println!("daily bars for {n} new symbols");
    Ok(())
}

/// Trial 7 (DESIGN §13c): research months only; logs the trial, then prints the gate.
fn momentum() -> Result<(), String> {
    use bmv_trader::momentum::{self as mo, Calendar};
    use bmv_trader::scan::ADJUSTED_SIP;
    let dir = data_dir()?;
    let cut = holdout_from(&dir, &[])?;
    let log = dir.join("trials.jsonl");
    // one trial (§13c): a rerun would log a second row and raise the bar for every later trial
    if std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .any(|l| l.contains(&format!("\"strategy\":\"{}\"", mo::TRIAL_NAME)))
    {
        return Err(format!(
            "{} is already in {}: trial 7 has run",
            mo::TRIAL_NAME,
            log.display()
        ));
    }
    let (from, to) = bmv_trader::scan::daily_range(&dir, ADJUSTED_SIP)?;
    if (from.as_str(), to.as_str()) != (mo::DATA_FROM, mo::DATA_TO) {
        return Err(format!(
            "adjusted bars cover {from}..{to}, but trial 7 is pre-registered on {}..{}",
            mo::DATA_FROM,
            mo::DATA_TO
        ));
    }
    eprintln!("adjusted SIP daily bars {from}..{to}; holdout from {cut} stays unseen");
    let spy = bmv_trader::scan::read_daily(&dir, ADJUSTED_SIP, mo::BENCHMARK)?;
    let cal = Calendar::new(&spy)?;
    let mut by_symbol = BTreeMap::new();
    let mut seen = 0;
    bmv_trader::scan::for_each_daily(&dir, ADJUSTED_SIP, &mut |s, bars| {
        by_symbol.insert(s.to_string(), mo::months(&bars, &cal));
        seen += 1;
        if seen % 2000 == 0 {
            eprintln!("  {seen} symbols");
        }
        Ok(())
    })?;
    let st = mo::study(&by_symbol, &cal, &cut)?;
    let period = format!(
        "{}..{}",
        st.dates.first().map_or("", String::as_str),
        st.dates.last().map_or("", String::as_str)
    );
    let trials = stats::count_trials(&log)? + 1;
    let trial = stats::Trial {
        when: chrono::Utc::now().to_rfc3339(),
        strategy: mo::TRIAL_NAME.into(),
        params: format!(
            "monthly, top {}, ${}M liquidity, {}% per side; units: expectancy_r and max_drawdown_r are monthly-return fractions, not R",
            mo::HOLD,
            mo::MIN_DOLLAR_VOLUME / 1e6,
            mo::COST_PER_SIDE * 100.0
        ),
        period: period.clone(),
        trades: st.strategy.len() * mo::HOLD,
        expectancy_r: mo::mean(&st.strategy),
        profit_factor: mo::profit_factor(&st.strategy),
        max_drawdown_r: mo::max_drawdown(&st.strategy),
        ci_low: st.ci_low(trials),
    };
    let logged = stats::record_trial(&log, &trial)?;
    println!(
        "trial {logged}: momentum 12-1, {period}, {} months",
        st.dates.len()
    );
    println!(
        "{:<16}{:>12}{:>10}{:>8}{:>12}",
        "", "mean/month", "CAGR", "Sharpe", "max DD"
    );
    for (name, r) in [
        ("momentum", &st.strategy),
        ("random 20", &st.baseline),
        ("SPY", &st.benchmark),
    ] {
        println!(
            "{name:<16}{:>11.2}%{:>9.1}%{:>8.2}{:>11.1}%",
            mo::mean(r) * 100.0,
            mo::cagr(r) * 100.0,
            mo::sharpe(r),
            mo::max_drawdown(r) * 100.0
        );
    }
    println!(
        "{} months had fewer than {} eligible names",
        st.thin_months,
        mo::HOLD
    );
    let mut most: Vec<(&String, &usize)> = st.held.iter().collect();
    most.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let most: Vec<String> = most
        .iter()
        .take(15)
        .map(|(s, n)| format!("{s} {n}"))
        .collect();
    println!("most-held names (months): {}", most.join(", "));
    let gate = st.gate(logged);
    for c in &gate.checks {
        println!(
            "  [{}] {}: {}",
            if c.passed { "pass" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    println!(
        "P1 gate: {}",
        if gate.passed() {
            "PASSED (the holdout is next)"
        } else {
            "not passed"
        }
    );
    Ok(())
}

/// Trial 8 (DESIGN §7d): research sessions only; logs the trial once, then prints the gate.
fn backtest_swing(rules: bmv_trader::swing::Rules) -> Result<(), String> {
    use bmv_trader::momentum::{self as mo, Calendar};
    use bmv_trader::scan::ADJUSTED_SIP;
    use bmv_trader::swing::{self as sw, Cand};
    let dir = data_dir()?;
    let cut_date = holdout_from(&dir, &[])?;
    let log = dir.join("trials.jsonl");
    if std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .any(|l| l.contains(&format!("\"strategy\":\"{}\"", rules.name)))
    {
        return Err(format!(
            "{} is already in {}: trial {} has run",
            rules.name,
            log.display(),
            rules.trial
        ));
    }
    let (from, to) = bmv_trader::scan::daily_range(&dir, ADJUSTED_SIP)?;
    if (from.as_str(), to.as_str()) != (mo::DATA_FROM, mo::DATA_TO) {
        return Err(format!(
            "adjusted bars cover {from}..{to}, but trial {} is pre-registered on {}..{}",
            rules.trial,
            mo::DATA_FROM,
            mo::DATA_TO
        ));
    }
    eprintln!("adjusted SIP daily bars {from}..{to}; holdout from {cut_date} stays unseen");
    let spy = bmv_trader::scan::read_daily(&dir, ADJUSTED_SIP, mo::BENCHMARK)?;
    let cal = Calendar::new(&spy)?;
    let asset_names = if rules.stocks_only {
        bmv_trader::scan::read_asset_names(&dir)?
    } else {
        BTreeMap::new()
    };
    let mut funds = 0;
    let mut names: Vec<String> = Vec::new();
    let mut days: Vec<Vec<Cand>> = vec![Vec::new(); cal.sessions.len()];
    bmv_trader::scan::for_each_daily(&dir, ADJUSTED_SIP, &mut |s, bars| {
        if rules.stocks_only && asset_names.get(s).is_some_and(|n| sw::is_fund(n)) {
            funds += 1;
            return Ok(());
        }
        for (d, c) in sw::candidates(names.len(), &bars, &cal, &rules) {
            days[d].push(c);
        }
        names.push(s.to_string());
        if names.len().is_multiple_of(2000) {
            eprintln!("  {} symbols", names.len());
        }
        Ok(())
    })?;
    let first = |d: &str| cal.sessions.iter().position(|s| s.as_str() >= d);
    let start = first(sw::RESEARCH_FROM).ok_or("no session after the research start")?;
    let cut = first(&cut_date).ok_or("no session at the holdout start")?;
    let (trades, per_day) = sw::portfolio(&days, &names, start, cut);
    let base = sw::baseline(&days, &per_day, cut);
    let st = sw::Study::new(trades, base, start..cut, &cal, &spy)?;
    let period = format!("{}..{}", cal.sessions[start], cal.sessions[cut - 1]);
    let trials = stats::count_trials(&log)? + 1;
    let ci_low = st.ci(trials).map(|(lo, _)| lo);
    let trial = stats::Trial {
        when: chrono::Utc::now().to_rfc3339(),
        strategy: rules.name.into(),
        params: format!(
            "EMA {:?} stack, {}-day high, volume {}x, RSI > {}, ATR < p{}; stop {}xATR, target {}xATR, {} sessions; max {} open; {}% per side",
            sw::EMAS,
            sw::BREAKOUT,
            sw::VOLUME_RATIO,
            sw::RSI_MIN,
            sw::ATR_MAX_PERCENTILE * 100.0,
            sw::STOP_ATR,
            sw::TARGET_ATR,
            sw::MAX_HOLD,
            sw::MAX_OPEN,
            sw::COST_PER_SIDE * 100.0
        ),
        period: period.clone(),
        trades: st.trades.len(),
        expectancy_r: st.expectancy(),
        profit_factor: st.profit_factor(),
        max_drawdown_r: st.max_drawdown_r(),
        ci_low,
    };
    if rules.stocks_only {
        println!("{funds} funds left out by name");
    }
    let logged = stats::record_trial(&log, &trial)?;
    let wins = st.trades.iter().filter(|t| t.out.r > 0.0).count();
    println!("trial {logged}: {}, {period}", rules.name);
    println!(
        "{} trades, {:.0}% winners, expectancy {:+.3}R; random entries {:+.3}R over {} trades",
        st.trades.len(),
        100.0 * wins as f64 / st.trades.len().max(1) as f64,
        st.expectancy(),
        st.baseline.iter().map(|t| t.out.r).sum::<f64>() / st.baseline.len().max(1) as f64,
        st.baseline.len()
    );
    println!(
        "at 1% risk per trade: CAGR {:+.1}%, Sharpe {:.2}, max DD {:.1}%; SPY CAGR {:+.1}%, Sharpe {:.2}, max DD {:.1}%",
        mo::cagr(&st.monthly) * 100.0,
        mo::sharpe(&st.monthly),
        mo::max_drawdown(&st.monthly) * 100.0,
        mo::cagr(&st.spy_monthly) * 100.0,
        mo::sharpe(&st.spy_monthly),
        mo::max_drawdown(&st.spy_monthly) * 100.0
    );
    let late = st
        .trades
        .iter()
        .filter(|t| t.out.exit > t.out.entry + sw::MAX_HOLD)
        .count();
    println!(
        "{late} trades exited after the time stop (missing bars or delisted); peak gross exposure {:.0}% of equity",
        st.peak_gross() * 100.0
    );
    let mut most: BTreeMap<&str, usize> = BTreeMap::new();
    for t in &st.trades {
        *most.entry(names[t.symbol].as_str()).or_default() += 1;
    }
    let mut most: Vec<(&str, usize)> = most.into_iter().collect();
    most.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top: Vec<String> = most
        .iter()
        .take(15)
        .map(|(s, n)| format!("{s} {n}"))
        .collect();
    println!("most-traded names: {}", top.join(", "));
    let gate = st.gate(logged);
    for c in &gate.checks {
        println!(
            "  [{}] {}: {}",
            if c.passed { "pass" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    println!(
        "P1 gate: {}",
        if gate.passed() {
            "PASSED (the holdout is next)"
        } else {
            "not passed"
        }
    );
    Ok(())
}

fn scan(from: &str, to: &str) -> Result<(), String> {
    let r = bmv_trader::scan::scan_range(&connect()?, &data_dir()?, from, to, &mut |d| {
        eprintln!("  {d}")
    })?;
    println!(
        "{} days, {} picks, {} picks without minute bars (kept as days without trades)",
        r.days, r.picks, r.no_minute_bars
    );
    Ok(())
}

fn run(symbols: &[String], trend: bool, orb: OrbParams) -> Result<(), String> {
    let cfg = config::load()?;
    let mode = cfg.mode();
    live::refuse_live(mode)?;
    eprintln!("mode: PAPER");
    let home = config::home()?;
    let _lock = live::lock_run(&home)?;
    let settings = Settings {
        vetting: false,
        orb,
        entry: Entry::Orb,
        trend_filter: trend,
    };
    let mut r = live::startup(
        broker::Alpaca::new(cfg)?,
        mode,
        &home,
        settings,
        symbols.to_vec(),
    )?;
    live::run(&mut r, &home)?;
    println!(
        "session done: pnl_today {}, halted {}",
        r.engine.pnl_today(),
        r.engine.halted()
    );
    Ok(())
}

fn data_dir() -> Result<std::path::PathBuf, String> {
    Ok(config::home()?.join("data"))
}

fn fetch(from: &str, to: &str, symbols: &[String], allow_gaps: bool) -> Result<(), String> {
    let today = chrono::Utc::now().date_naive();
    if history::canonical_date(to)? >= today {
        return Err(format!(
            "--to must be before today ({today}) so no day is partial"
        ));
    }
    let alpaca = connect()?;
    eprintln!(
        "fetching {} symbols, {from} .. {to}, feed {}",
        symbols.len(),
        history::FEED
    );
    let r = history::fetch_range(
        &alpaca,
        &data_dir()?,
        symbols,
        from,
        to,
        allow_gaps,
        &mut |d| eprintln!("  {d}"),
    )?;
    println!(
        "{} sessions; {} symbol-days downloaded, {} already cached",
        r.sessions, r.downloaded, r.cached
    );
    for (date, sym) in &r.gaps {
        println!(
            "  gap: no {} bars for {sym} on {date}, stored as a day without trades",
            history::FEED
        );
    }
    Ok(())
}

struct Opts {
    fold_days: usize,
    seeds: usize,
    holdout: bool,
    holdout_again: bool,
    /// Only trade while SPY and the symbol trend up today; a separate trial from plain ORB.
    trend: bool,
    /// Trade each day's scanned gappers (`bmv-trader scan`) instead of `--symbols`.
    scan: bool,
    symbols_given: bool,
    orb: OrbParams,
    strategy: Strategy,
}

/// Which entry rule a backtest trial uses (DESIGN §7, §7b, §7c). Exits and sizing are shared.
#[derive(Clone, Copy, Debug)]
enum Strategy {
    Orb,
    Gap,
    Vwap,
}

impl Strategy {
    fn entry(self) -> Entry {
        match self {
            Strategy::Orb => Entry::Orb,
            Strategy::Gap => Entry::Gap,
            Strategy::Vwap => Entry::VwapPullback,
        }
    }
    /// The trial name prefix in data/trials.jsonl.
    fn name(self) -> &'static str {
        match self {
            Strategy::Orb => "orb",
            Strategy::Gap => "gap",
            Strategy::Vwap => "vwap",
        }
    }
}

/// The one pre-registered entry variant: a fixed buffer, not a tunable knob, so it's one trial.
const MARKETABLE_BUFFER_BPS: i64 = 5;

fn orb_params(args: &Args) -> OrbParams {
    OrbParams {
        entry_buffer_bps: if args.has("--marketable") {
            MARKETABLE_BUFFER_BPS
        } else {
            0
        },
        ..OrbParams::default()
    }
}

/// The pinned holdout start: read from `data/holdout_from`, or set once from the loaded days.
fn holdout_from(dir: &Path, dates: &[&str]) -> Result<String, String> {
    let path = dir.join("holdout_from");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        history::canonical_date(&s).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(s);
    }
    if dates.len() <= HOLDOUT_SESSIONS {
        return Err(format!(
            "{} sessions loaded: need more than {HOLDOUT_SESSIONS} to set aside a holdout",
            dates.len()
        ));
    }
    let pinned = dates[dates.len() - HOLDOUT_SESSIONS].to_string();
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, format!("{pinned}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    eprintln!(
        "holdout pinned: sessions from {pinned} on (in {}); research stays before it",
        path.display()
    );
    Ok(pinned)
}

/// Walk-forward test windows of `fold_days`; a short last window joins the one before it.
fn folds(results: &[DayResult], fold_days: usize) -> Vec<Vec<DayResult>> {
    let mut folds: Vec<Vec<DayResult>> = results
        .chunks(fold_days)
        .map(<[DayResult]>::to_vec)
        .collect();
    if folds.len() > 1 && folds.last().is_some_and(|f| f.len() < fold_days) {
        let tail = folds.pop().unwrap_or_default();
        if let Some(prev) = folds.last_mut() {
            prev.extend(tail);
        }
    }
    folds
}

fn run_backtest(from: &str, to: &str, symbols: &[String], o: &Opts) -> Result<(), String> {
    if o.scan && o.symbols_given {
        return Err("--scan trades each day's scanned gappers; drop --symbols".into());
    }
    if o.scan && o.trend {
        return Err("--trend needs SPY, which the scan doesn't trade: use one or the other".into());
    }
    if o.trend && !symbols.iter().any(|s| s == engine::MARKET) {
        return Err(format!(
            "--trend reads {} to judge the market; add it to --symbols",
            engine::MARKET
        ));
    }
    let dir = data_dir()?;
    let days = if o.scan {
        bmv_trader::scan::load_scan_range(&dir, from, to)?
    } else {
        history::load_range(&dir, symbols, from, to)?
    };
    let dates: Vec<&str> = days.iter().map(|d| d.date.as_str()).collect();
    let cut_date = holdout_from(&dir, &dates)?;
    let cut = days.partition_point(|d| d.date < cut_date);
    if cut == 0 {
        return Err(format!(
            "every loaded day is in the holdout (from {cut_date}): start earlier"
        ));
    }
    let (research_days, holdout_days) = days.split_at(cut);
    let log = dir.join("trials.jsonl");
    let holdout_period = holdout_days
        .first()
        .zip(holdout_days.last())
        .map(|(a, b)| format!("{}..{}", a.date, b.date));
    if o.holdout {
        let period = holdout_period.as_deref().ok_or(format!(
            "no loaded days on or after the holdout start {cut_date}"
        ))?;
        let seen = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            // any strategy's look at this holdout spends it
            .any(|l| l.contains("-holdout\"") && l.contains(&cut_date));
        if seen && !o.holdout_again {
            return Err(format!(
                "the holdout from {cut_date} was already evaluated (see {}); pass --holdout-again only if you mean to spend it twice",
                log.display()
            ));
        }
        eprintln!("evaluating the holdout {period}");
    }

    let orb = o.orb.clone();
    let settings = |entry| Settings {
        vetting: false,
        orb: orb.clone(),
        entry,
        trend_filter: o.trend,
    };
    let span = if o.holdout { &days[..] } else { research_days };
    let results = backtest::run(span, settings(o.strategy.entry()), FillModel::default())?;
    let (research, holdout) = results.split_at(cut);
    let mut baseline = Vec::new();
    for seed in 1..=o.seeds as u64 {
        baseline.extend(backtest::run(
            research_days,
            settings(Entry::Random { seed }),
            FillModel::default(),
        )?);
    }

    let s = backtest::summarize(research);
    let ci = stats::bootstrap_expectancy(&stats::r_by_day(research), 2_000, 0.05, 42);
    let period = format!("{}..{}", research_days[0].date, research_days[cut - 1].date);
    let when = chrono::Utc::now().to_rfc3339();
    let params = format!(
        "{:?} {orb:?} trend_filter={} scan={}",
        o.strategy, o.trend, o.scan
    );
    let strategy_name = format!(
        "{}{}{}{}",
        o.strategy.name(),
        if o.scan { "@scan" } else { "" },
        if o.trend { "+trend" } else { "" },
        if o.orb.entry_buffer_bps > 0 {
            "+mkt"
        } else {
            ""
        }
    );
    let strategy_name = strategy_name.as_str();
    stats::record_trial(
        &log,
        &stats::Trial::from_summary(&when, strategy_name, &params, &period, &s, ci.map(|c| c.0)),
    )?;
    let holdout_results = o.holdout.then_some(holdout);
    if let (Some(h), Some(hp)) = (holdout_results, &holdout_period) {
        let hs = backtest::summarize(h);
        let label = format!("{hp} (holdout from {cut_date})");
        stats::record_trial(
            &log,
            &stats::Trial::from_summary(
                &when,
                &format!("{strategy_name}-holdout"),
                &params,
                &label,
                &hs,
                None,
            ),
        )?;
    }
    let report = stats::p1_gate(
        &folds(research, o.fold_days),
        &baseline,
        holdout_results,
        &log,
    )?;

    let b = backtest::summarize(&baseline);
    println!(
        "research {period}: {cut} days, {} trades, {} missed entries",
        s.trades, s.missed
    );
    println!(
        "  win rate {:.1}%  expectancy {:.3}R  profit factor {:.2}  max drawdown {:.1}R  P&L {}",
        100.0 * s.win_rate,
        s.expectancy_r,
        s.profit_factor,
        s.max_drawdown_r,
        s.pnl
    );
    println!(
        "random baseline ({} seeds): {} trades, expectancy {:.3}R",
        o.seeds, b.trades, b.expectancy_r
    );
    if let Some(h) = holdout_results {
        let hs = backtest::summarize(h);
        println!(
            "HOLDOUT {}: {} trades, expectancy {:.3}R",
            holdout_period.unwrap_or_default(),
            hs.trades,
            hs.expectancy_r
        );
    }
    println!(
        "\nP1 gate ({} trials logged in {}):",
        stats::count_trials(&log)?,
        log.display()
    );
    for c in &report.checks {
        println!(
            "  [{}] {}: {}",
            if c.passed { "pass" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    println!(
        "{}",
        if report.passed() {
            "P1 GATE PASSED"
        } else {
            "P1 gate not passed"
        }
    );
    Ok(())
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(1)
}
