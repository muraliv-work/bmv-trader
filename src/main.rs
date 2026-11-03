use bmv_trader::backtest::{self, DayResult, FillModel};
use bmv_trader::engine::{self, Entry, Settings};
use bmv_trader::strategy::OrbParams;
use bmv_trader::{broker, config, config::Mode, history, live, stats};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::exit;

const USAGE: &str = "usage:
  bmv-trader account
  bmv-trader status                       account, positions and open orders (read-only)
  bmv-trader watch                        live read-only monitor, refreshes every 5 s (Ctrl-C quits)
  bmv-trader run      [--symbols ...] [--trend] [--marketable]   one paper session (PAPER ONLY; live is P5)
  bmv-trader flatten                      close every position and confirm (any mode)
  bmv-trader fetch    --from YYYY-MM-DD --to YYYY-MM-DD [--symbols SPY,QQQ,...] [--allow-gaps]
  bmv-trader backtest --from YYYY-MM-DD --to YYYY-MM-DD [--symbols ...] [--fold-days 21]
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
const VALUE_FLAGS: [&str; 5] = ["--from", "--to", "--symbols", "--fold-days", "--seeds"];
const SWITCHES: [&str; 5] = [
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
            Some("flatten") => flatten(),
            Some("run") => run(&args.symbols()?, args.has("--trend"), orb_params(&args)),
            Some("fetch") => {
                let (from, to) = args.dates()?;
                fetch(&from, &to, &args.symbols()?, args.has("--allow-gaps"))
            }
            Some("backtest") => {
                let (from, to) = args.dates()?;
                let opts = Opts {
                    fold_days: args.num("--fold-days", 21, 5..=126)?,
                    seeds: args.num("--seeds", 5, 1..=MAX_SEEDS)?,
                    holdout: args.has("--holdout"),
                    holdout_again: args.has("--holdout-again"),
                    trend: args.has("--trend"),
                    orb: orb_params(&args),
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

/// Read-only monitor. Alpaca errors are shown and retried next refresh, never fatal.
fn watch() -> Result<(), String> {
    use bmv_trader::{safety, watch};
    let mode = format!("{:?}", config::load()?.mode()).to_uppercase();
    let a = connect()?;
    let home = config::home()?;
    let (journal, stop) = (live::journal_path(&home), safety::stop_path(&home));
    loop {
        let mut out = String::from("\x1b[2J\x1b[H"); // clear screen, cursor home
        let now = chrono::Utc::now().timestamp();
        let text = match std::fs::read_to_string(&journal) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => format!("{{\"unreadable journal\": \"{e}\"}}"),
        };
        let s = watch::summarize(&text, 12);
        let stop_file = match safety::kill_switch(&stop) {
            safety::KillSwitch::Clear => "clear".to_string(),
            safety::KillSwitch::Engaged(why) => format!("ENGAGED ({why})"),
        };
        out += &format!(
            "bmv-trader watch · {} ET · {mode} · read-only (Ctrl-C quits)\n\n",
            watch::et(now)
        );
        out += &format!(
            "  state: {}   session: {}   STOP file: {stop_file}\n",
            s.state(),
            s.session.as_deref().unwrap_or("-")
        );
        if let Some(why) = &s.halted {
            out += &format!("  halt reason: {why}\n");
        }
        if !s.unresolved.is_empty() {
            let keys: Vec<&str> = s.unresolved.iter().map(String::as_str).collect();
            out += &format!("  unresolved: {}\n", keys.join(", "));
        }
        if s.unreadable > 0 {
            out += &format!("  WARNING: {} unreadable journal lines\n", s.unreadable);
        }
        out += &format!(
            "  today: {} candidates · {} rejected · {} orders · {} fills · {} exits\n\n",
            s.candidates, s.rejections, s.orders, s.fills, s.exits
        );
        match a.account() {
            Ok(acct) => {
                out += &format!(
                    "  equity {}   today P&L {}   buying power {}\n",
                    acct.equity,
                    acct.equity - acct.last_equity,
                    acct.buying_power
                )
            }
            Err(e) => out += &format!("  account: ERROR {e}\n"),
        }
        match a.positions() {
            Ok(ps) if ps.is_empty() => out += "  positions: none\n",
            Ok(ps) => {
                out += "  positions:\n";
                for p in ps {
                    out += &format!("    {:>6} {:>6} @ {}\n", p.symbol, p.qty, p.avg_entry_price);
                }
            }
            Err(e) => out += &format!("  positions: ERROR {e}\n"),
        }
        match live::open_orders(&a) {
            Ok(os) if os.is_empty() => out += "  open orders: none\n",
            Ok(os) => {
                out += "  open orders:\n";
                for o in os {
                    out += &format!(
                        "    {} {} {} {} {}/{}\n",
                        o.client_order_id, o.symbol, o.side, o.status, o.filled_qty, o.qty
                    );
                }
            }
            Err(e) => out += &format!("  open orders: ERROR {e}\n"),
        }
        out += "\n  recent journal:\n";
        for l in &s.recent {
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

/// The emergency exit, allowed in both modes.
fn flatten() -> Result<(), String> {
    let a = connect()?;
    live::flatten(&a, 10, &mut || {
        std::thread::sleep(std::time::Duration::from_secs(1))
    })?;
    println!("flat");
    Ok(())
}

fn run(symbols: &[String], trend: bool, orb: OrbParams) -> Result<(), String> {
    let cfg = config::load()?;
    let mode = cfg.mode();
    live::refuse_live(mode)?;
    eprintln!("mode: PAPER");
    let home = config::home()?;
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
    orb: OrbParams,
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
    if o.trend && !symbols.iter().any(|s| s == engine::MARKET) {
        return Err(format!(
            "--trend reads {} to judge the market; add it to --symbols",
            engine::MARKET
        ));
    }
    let dir = data_dir()?;
    let days = history::load_range(&dir, symbols, from, to)?;
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
            .any(|l| l.contains("\"orb-holdout\"") && l.contains(&cut_date));
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
    let results = backtest::run(span, settings(Entry::Orb), FillModel::default())?;
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
    let params = format!("{orb:?} trend_filter={}", o.trend);
    let strategy_name = format!(
        "orb{}{}",
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
