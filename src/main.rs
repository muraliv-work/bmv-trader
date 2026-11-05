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
  bmv-trader power                        the smallest effect trials 11 and 12 could detect (spreads from trial 7)
  bmv-trader lab-search [--minutes 5] [--seed 1]  evolve ranking formulas on the search months only (DESIGN §7h)
  bmv-trader trial-autopsy                every finished trial, reproduced and broken down: data/lessons.jsonl (DESIGN §13d)
  bmv-trader stress-battery               every finished trial reproduced twice with run hashes, then stress-tested: data/stress.jsonl
  bmv-trader retro-power                  trial 12's power gate: research-period signals only (DESIGN §7i)
  bmv-trader retro-sample                 trial 12: freeze each untouched-year session's top signal (once)
  bmv-trader retro-speed [--count 3]      the local analyst on 2024 signals: minutes per run (outcomes unused)
  bmv-trader retro-run [--hours 8]        trial 12: analyse the frozen sample with the local model; resumes
  bmv-trader retro-report                 trial 12 so far: counts and ratings only
  bmv-trader retro-verdict                trial 12's verdict, once every signal is answered (logged once)
  bmv-trader lab-validate                 ONCE: freeze the top 20 distinct formulas, score them on 2023-01..2025-09, log trial 11
  bmv-trader lab-ideas [--count 10]       ask the local model (Tier 2) for new formulas; the next lab-search starts from them
  bmv-trader desk-run                     trial 10 nightly (20 min after the close): live bars, signals, analyst ratings, scores
  bmv-trader desk-report                  trial 10 so far: signals, ratings, spend, outcomes
  bmv-trader desk-analyse --symbols X --date YYYY-MM-DD  one analyst run outside the trial (measures cost)
  bmv-trader desk-verdict                 trial 10's pre-registered verdict, once it is due (logged once)
  bmv-trader notes                        Tier 2: write any due notes (desk brief, analyst digest, session, Sunday digest)
  bmv-trader llm-health                   check Ollama, OmniRoute, a Tier-2 probe, key age and Claude spend
  bmv-trader llm-rotate                   regenerate the OmniRoute key, store it, re-apply its policy
  bmv-trader llm-daily                    for launchd: llm-rotate, then llm-health
  bmv-trader free-keys                    rotate the free cloud models' OpenRouter key if due, retire old ones (S42)
  bmv-trader free-keys --record PROVIDER  log a key pasted with scripts/free-key-add.sh, for its expiry
  bmv-trader free-keys --connection PROVIDER  a pasted provider's OmniRoute connection, and new|existing
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
const VALUE_FLAGS: [&str; 15] = [
    "--date",
    "--trial",
    "--strategy",
    "--from",
    "--to",
    "--symbols",
    "--fold-days",
    "--seeds",
    "--port",
    "--minutes",
    "--seed",
    "--count",
    "--hours",
    "--record",
    "--connection",
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
            Some("power") => power(),
            Some("lab-validate") => lab_validate(),
            Some("retro-power") => retro_power(),
            Some("trial-autopsy") => trial_autopsy(),
            Some("stress-battery") => stress_battery(),
            Some("retro-sample") => retro_sample(),
            Some("retro-speed") => retro_speed(args.num("--count", 3, 1..=20)?),
            Some("retro-run") => retro_run(args.num("--hours", 8, 1..=72)?),
            Some("retro-report") => retro_report(),
            Some("retro-verdict") => retro_verdict(),
            Some("lab-ideas") => lab_ideas(args.num("--count", 10, 1..=100)?),
            Some("lab-search") => lab_search(
                args.num("--minutes", 5, 1..=600)?,
                args.num("--seed", 1, 1..=1_000_000)? as u64,
            ),
            Some("desk-run") => desk_run(),
            Some("desk-report") => {
                let (home, dir) = (config::home()?, data_dir()?);
                desk_report(&home, &live_calendar(&dir)?).map(|_| ())
            }
            Some("desk-verdict") => desk_verdict(),
            Some("desk-analyse") => {
                let symbol = args.symbols()?;
                match (symbol.as_slice(), args.get("--date")) {
                    ([s], Some(d)) if args.get("--symbols").is_some() => desk_analyse(s, d),
                    _ => Err("desk-analyse needs --symbols X (one) and --date YYYY-MM-DD".into()),
                }
            }
            Some("notes") => notes(),
            Some("llm-health") => llm_health().map(|_| ()),
            Some("llm-rotate") => llm_rotate(),
            Some("free-keys") => free_keys(args.get("--record"), args.get("--connection")),
            Some("llm-daily") => {
                let rotated = llm_rotate();
                if let Err(e) = &rotated {
                    eprintln!("rotation failed: {e}");
                }
                let healthy = llm_health()?;
                rotated?;
                if healthy {
                    Ok(())
                } else {
                    Err("Tier 2 health check failed (data/llm_health.json)".into())
                }
            }
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
/// The lab's panel (DESIGN §7h): trial 7's bars and calendar, stocks only, labels before `until`.
fn lab_panel(
    dir: &Path,
    until: &str,
) -> Result<(bmv_trader::momentum::Calendar, bmv_trader::lab::Panel), String> {
    use bmv_trader::momentum::{self as mo, Calendar};
    use bmv_trader::scan::ADJUSTED_SIP;
    let (from, to) = bmv_trader::scan::daily_range(dir, ADJUSTED_SIP)?;
    if (from.as_str(), to.as_str()) != (mo::DATA_FROM, mo::DATA_TO) {
        return Err(format!(
            "adjusted bars cover {from}..{to}, but the lab is pre-registered on {}..{}",
            mo::DATA_FROM,
            mo::DATA_TO
        ));
    }
    let spy = bmv_trader::scan::read_daily(dir, ADJUSTED_SIP, mo::BENCHMARK)?;
    let cal = Calendar::new(&spy)?;
    let names = bmv_trader::scan::read_asset_names(dir)?;
    let mut panel = bmv_trader::lab::Panel::new(&cal, until);
    let mut seen = 0;
    bmv_trader::scan::for_each_daily(dir, ADJUSTED_SIP, &mut |s, bars| {
        seen += 1;
        if seen % 2000 == 0 {
            eprintln!("  {seen} symbols");
        }
        if s == mo::BENCHMARK || names.get(s).is_some_and(|n| bmv_trader::swing::is_fund(n)) {
            return Ok(());
        }
        panel.add(s, &bars, &cal);
        Ok(())
    })?;
    Ok((cal, panel))
}

fn append_jsonl<T: serde::Serialize>(path: &Path, rows: &[T]) -> Result<(), String> {
    use std::io::Write;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = String::new();
    for r in rows {
        text.push_str(&serde_json::to_string(r).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    f.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Local-model formula ideas (DESIGN §7h, DECISION S25). It sees the feature list and the best
/// search-period formulas only; its reply is used only where a line parses as a formula.
fn lab_ideas(count: usize) -> Result<(), String> {
    use bmv_trader::lab::{self, Candidate, Expr};
    use bmv_trader::llm::{self, Tier2Task};
    let lab_dir = data_dir()?.join("lab");
    if lab_dir.join("validated").exists() {
        return Err("the lab has been validated: the search is closed (DESIGN §7h)".into());
    }
    let read = |name: &str| std::fs::read_to_string(lab_dir.join(name)).unwrap_or_default();
    let mut tried: Vec<Candidate> = read("candidates.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    tried.sort_by(|a, b| b.stats.fitness.total_cmp(&a.stats.fitness));
    let mut known: std::collections::HashSet<String> =
        tried.iter().map(|c| c.key.clone()).collect();
    for l in read("ideas.jsonl").lines() {
        if let Ok(e) = Expr::parse(l) {
            known.insert(e.key());
        }
    }
    let mut facts = String::from("Features:\n");
    for f in lab::FEATURES {
        let name = serde_json::to_string(&f).map_err(|e| e.to_string())?;
        facts.push_str(&format!("- {}: {}\n", name.trim_matches('"'), f.meaning()));
    }
    facts.push_str("\nBest formulas so far (search period; IC is the monthly rank correlation with next month's return):\n");
    for c in tried.iter().take(10) {
        facts.push_str(&format!(
            "- {} (IC {:.3})\n",
            serde_json::to_string(&c.expr).map_err(|e| e.to_string())?,
            c.stats.ic_mean
        ));
    }
    let (mut new, mut duplicate, mut rejected, mut calls) = (Vec::new(), 0, 0, 0);
    while new.len() < count && calls < count.div_ceil(2) + 3 {
        calls += 1;
        let reply = match llm::complete(Tier2Task::LabIdea, &facts) {
            Ok(n) => n.0,
            Err(e) => {
                eprintln!("call {calls}: {e}");
                continue;
            }
        };
        for line in reply.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match Expr::parse(line) {
                Ok(e) if known.insert(e.key()) => {
                    println!("  new: {}", e.key());
                    new.push(e);
                }
                Ok(_) => duplicate += 1,
                Err(_) => rejected += 1,
            }
            if new.len() >= count {
                break;
            }
        }
    }
    append_jsonl(&lab_dir.join("ideas.jsonl"), &new)?;
    println!(
        "{} new formulas from {calls} local-model calls ({duplicate} already known, {rejected} lines rejected); \
         the next lab-search starts from them",
        new.len()
    );
    Ok(())
}

/// Trial 11's validation stage (DESIGN §7h): run once. The picks are frozen and written before any
/// validation month is scored, and the lab's search closes for good.
fn lab_validate() -> Result<(), String> {
    use bmv_trader::lab::{self, Candidate};
    let dir = data_dir()?;
    let lab_dir = dir.join("lab");
    let done = lab_dir.join("validated");
    if done.exists() {
        return Err(format!(
            "{} exists: validation runs once (DESIGN §7h)",
            done.display()
        ));
    }
    let log = dir.join("trials.jsonl");
    if logged_trials(&log)?.iter().any(|t| t == lab::TRIAL_11) {
        return Err("trial 11 is already logged".into());
    }
    let read = |name: &str| std::fs::read_to_string(lab_dir.join(name)).unwrap_or_default();
    let cands: Vec<Candidate> = read("candidates.jsonl")
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("candidates.jsonl: {e}"))?;
    let tries: usize = read("runs.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["tries"].as_u64())
        .sum::<u64>() as usize;
    if cands.is_empty() {
        return Err("no candidates: run lab-search first".into());
    }
    let (cal, panel) = lab_panel(&dir, lab::VALIDATION_END)?;
    let search = panel.range(&cal, lab::SEARCH_START, lab::SEARCH_END);
    let validation = panel.range(&cal, lab::SEARCH_END, lab::VALIDATION_END);
    // distinctness from search months only (every 6th, for speed)
    let sample: Vec<usize> = search.iter().copied().step_by(6).collect();
    let picks = lab::distinct(&cands, &panel, &sample, lab::VALIDATE);
    append_jsonl(&lab_dir.join("frozen.jsonl"), &picks)?;
    eprintln!(
        "{} picks frozen from {} candidates ({tries} tries); scoring {} validation months ({}..{})",
        picks.len(),
        cands.len(),
        validation.len(),
        validation.first().map_or("", |&k| panel.dates[k].as_str()),
        validation.last().map_or("", |&k| panel.dates[k].as_str()),
    );
    let mut rows = Vec::new();
    for c in &picks {
        let v = lab::evaluate(&c.expr, &panel, &validation)
            .ok_or_else(|| format!("{}: too few validation months", c.key))?;
        let bound = lab::ic_bound(&v.ics, lab::TRIAL_11_NUMBER);
        rows.push((c, v, bound));
    }
    let advanced: Vec<&Candidate> = rows
        .iter()
        .filter(|(_, v, b)| b.is_some_and(|b| b > 0.0) && v.excess_mean > 0.0)
        .map(|(c, _, _)| *c)
        .take(lab::ADVANCE)
        .collect();
    append_jsonl(&lab_dir.join("advanced.jsonl"), &advanced)?;
    let best = rows
        .iter()
        .max_by(|a, b| a.2.unwrap_or(f64::MIN).total_cmp(&b.2.unwrap_or(f64::MIN)))
        .ok_or("no picks")?;
    let trial = stats::Trial {
        when: chrono::Utc::now().to_rfc3339(),
        strategy: lab::TRIAL_11.into(),
        params: format!(
            "{tries} search tries, {} validation picks, {} advanced; units: expectancy_r is the best pick's mean monthly IC and ci_low its 3-month-block bound at 1 - 0.05/11, not R; max_drawdown_r unused",
            picks.len(),
            advanced.len()
        ),
        period: format!(
            "{}..{}",
            validation.first().map_or("", |&k| panel.dates[k].as_str()),
            validation.last().map_or("", |&k| panel.dates[k].as_str())
        ),
        trades: validation.len() * lab::HOLD,
        expectancy_r: best.1.ic_mean,
        profit_factor: None,
        max_drawdown_r: 0.0,
        ci_low: best.2,
    };
    let n = stats::record_trial(&log, &trial)?;
    std::fs::write(&done, format!("{}\n", trial.when))
        .map_err(|e| format!("{}: {e}", done.display()))?;
    let sd = bmv_trader::momentum::mean(&rows.iter().map(|r| r.0.stats.ic_sd).collect::<Vec<_>>());
    println!(
        "trial {} logged as \"{}\" (row {n} of the log; trial 10's verdict comes later). Search luck line from {tries} tries: {:.3}",
        lab::TRIAL_11_NUMBER,
        lab::TRIAL_11,
        lab::luck_line(sd, search.len(), tries)
    );
    println!(
        "{:>9} {:>8} {:>9} {:>10}  formula",
        "search IC", "val IC", "IC bound", "excess/mo"
    );
    for (c, v, b) in &rows {
        println!(
            "{:>9.3} {:>8.3} {:>9} {:>9.2}%  {}{}",
            c.stats.ic_mean,
            v.ic_mean,
            b.map_or("none".into(), |b| format!("{b:.3}")),
            v.excess_mean * 100.0,
            c.key,
            if advanced.iter().any(|a| a.key == c.key) {
                "  ADVANCES"
            } else {
                ""
            }
        );
    }
    println!(
        "{} of {} advance to the untouched-year check (lab-final). The search is closed.",
        advanced.len(),
        picks.len()
    );
    Ok(())
}

/// Trial 11's search stage (DESIGN §7h): only months whose labels end before 2023 are loaded.
fn lab_search(minutes: usize, seed: u64) -> Result<(), String> {
    use bmv_trader::lab::{self, Candidate, Expr};
    let dir = data_dir()?;
    let lab_dir = dir.join("lab");
    if lab_dir.join("validated").exists() {
        return Err("the lab has been validated: the search is closed (DESIGN §7h)".into());
    }
    let t0 = std::time::Instant::now();
    let (cal, panel) = lab_panel(&dir, lab::SEARCH_END)?;
    let months = panel.range(&cal, lab::SEARCH_START, lab::SEARCH_END);
    let rows: usize = months.iter().map(|&k| panel.months[k].len()).sum();
    eprintln!(
        "panel: {} symbols, {} search months ({}..{}), {rows} rows, built in {:.0} s",
        panel.symbols.len(),
        months.len(),
        months.first().map_or("", |&k| panel.dates[k].as_str()),
        months.last().map_or("", |&k| panel.dates[k].as_str()),
        t0.elapsed().as_secs_f64()
    );
    // earlier bests and parsed local-model ideas start the population
    let mut seeds: Vec<(Expr, String)> = Vec::new();
    let read = |name: &str| std::fs::read_to_string(lab_dir.join(name)).unwrap_or_default();
    let mut old: Vec<Candidate> = read("candidates.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    old.sort_by(|a, b| b.stats.fitness.total_cmp(&a.stats.fitness));
    seeds.extend(
        old.into_iter()
            .take(50)
            .map(|c| (c.expr, "earlier".to_string())),
    );
    for l in read("ideas.jsonl").lines() {
        if let Ok(e) = Expr::parse(l) {
            seeds.push((e, "llm".into()));
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(minutes as u64 * 60);
    let mut batch: Vec<Candidate> = Vec::new();
    let path = lab_dir.join("candidates.jsonl");
    let mut write_err = None;
    let started = std::time::Instant::now();
    let (pop, tries) = lab::search(
        &panel,
        &months,
        seeds,
        seed,
        &mut |_| std::time::Instant::now() < deadline,
        &mut |c| {
            batch.push(c.clone());
            if batch.len() >= 500 {
                if let Err(e) = append_jsonl(&path, &batch) {
                    write_err = Some(e);
                }
                batch.clear();
            }
        },
    );
    append_jsonl(&path, &batch)?;
    if let Some(e) = write_err {
        return Err(e);
    }
    let secs = started.elapsed().as_secs_f64();
    append_jsonl(
        &lab_dir.join("runs.jsonl"),
        &[serde_json::json!({
            "when": chrono::Utc::now().to_rfc3339(),
            "seed": seed,
            "minutes": minutes,
            "tries": tries,
        })],
    )?;
    let all_tries: usize = read("runs.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["tries"].as_u64())
        .sum::<u64>() as usize;
    let sd = bmv_trader::momentum::mean(
        &pop.iter()
            .take(20)
            .map(|c| c.stats.ic_sd)
            .collect::<Vec<_>>(),
    );
    println!(
        "{tries} tries in {secs:.0} s ({:.0}/s); {all_tries} tries across all runs",
        tries as f64 / secs.max(0.001)
    );
    println!(
        "luck line: the best search IC expected by chance from {all_tries} tries is about {:.3}",
        lab::luck_line(sd, months.len(), all_tries)
    );
    println!(
        "{:>8} {:>8} {:>10} {:>9}  formula",
        "IC", "fitness", "excess/mo", "turnover"
    );
    for c in pop.iter().take(5) {
        println!(
            "{:>8.3} {:>8.3} {:>9.2}% {:>8.0}%  {}",
            c.stats.ic_mean,
            c.stats.fitness,
            c.stats.excess_mean * 100.0,
            c.stats.turnover * 100.0,
            c.key
        );
    }
    Ok(())
}

/// Trial 7's calendar and every symbol's monthly score and return, from the pre-registered bars.
fn trial7_months(
    dir: &Path,
    cut: &str,
) -> Result<
    (
        bmv_trader::momentum::Calendar,
        BTreeMap<String, Vec<bmv_trader::momentum::Month>>,
    ),
    String,
> {
    use bmv_trader::momentum::{self as mo, Calendar};
    use bmv_trader::scan::ADJUSTED_SIP;
    let (from, to) = bmv_trader::scan::daily_range(dir, ADJUSTED_SIP)?;
    if (from.as_str(), to.as_str()) != (mo::DATA_FROM, mo::DATA_TO) {
        return Err(format!(
            "adjusted bars cover {from}..{to}, but trial 7 is pre-registered on {}..{}",
            mo::DATA_FROM,
            mo::DATA_TO
        ));
    }
    eprintln!("adjusted SIP daily bars {from}..{to}; holdout from {cut} stays unseen");
    let spy = bmv_trader::scan::read_daily(dir, ADJUSTED_SIP, mo::BENCHMARK)?;
    let cal = Calendar::new(&spy)?;
    let mut by_symbol = BTreeMap::new();
    let mut seen = 0;
    bmv_trader::scan::for_each_daily(dir, ADJUSTED_SIP, &mut |s, bars| {
        by_symbol.insert(s.to_string(), mo::months(&bars, &cal));
        seen += 1;
        if seen % 2000 == 0 {
            eprintln!("  {seen} symbols");
        }
        Ok(())
    })?;
    Ok((cal, by_symbol))
}

/// Research plan step 2: how big an edge trials 11 and 12 could detect. Uses trial 7's spreads
/// only (its months are already spent); logs nothing and prints no new mean.
fn power() -> Result<(), String> {
    use bmv_trader::momentum as mo;
    use bmv_trader::power as pw;
    let dir = data_dir()?;
    let cut = holdout_from(&dir, &[])?;
    let (cal, by_symbol) = trial7_months(&dir, &cut)?;
    let st = mo::study(&by_symbol, &cal, &cut)?;
    let excess = st.excess();
    println!(
        "spread guide: trial 7, {} research months, monthly excess over random-20 sd {:.2}%",
        excess.len(),
        100.0 * pw::sd(&excess)
    );
    println!("\ntrial 11 (earnings drift), gate at 1 - 0.05/11, 3-month blocks:");
    println!("  months  power  smallest monthly excess  (a year)");
    for months in [115, 80] {
        let se = pw::block_se(&excess, mo::BLOCK_MONTHS, months);
        for power in [0.5, 0.8] {
            let d = pw::detectable(se, 11, power);
            println!(
                "  {months:>6}  {:>4.0}%  {:>22.2}%  ({:.1}%)",
                power * 100.0,
                d * 100.0,
                d * 1200.0
            );
        }
    }
    let k0 = (0..cal.rebalances.len())
        .find(|&k| st.dates.first().is_some_and(|d| cal.date(k) == d))
        .ok_or("trial 7's first month isn't in the calendar")?;
    let ic = pw::monthly_ic(&by_symbol, k0..k0 + st.dates.len());
    println!(
        "\ntrial 12 (ML ranking), gate at 1 - 0.05/12: monthly IC sd {:.3} over {} months (12-1 score)",
        pw::sd(&ic),
        ic.len()
    );
    println!("  months  power  smallest mean IC");
    for months in [69] {
        let se = pw::block_se(&ic, mo::BLOCK_MONTHS, months);
        for power in [0.5, 0.8] {
            println!(
                "  {months:>6}  {:>4.0}%  {:>16.3}",
                power * 100.0,
                pw::detectable(se, 12, power)
            );
        }
    }
    Ok(())
}

fn momentum() -> Result<(), String> {
    use bmv_trader::momentum as mo;
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
    let (cal, by_symbol) = trial7_months(&dir, &cut)?;
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
/// Every session's swing candidates under `rules` (trials 8, 9 and 12): the calendar, the
/// symbols, candidates by signal session, and the research sessions [start, cut).
#[allow(clippy::type_complexity)]
fn swing_days(
    dir: &Path,
    rules: &bmv_trader::swing::Rules,
    cut_date: &str,
) -> Result<
    (
        bmv_trader::momentum::Calendar,
        Vec<String>,
        Vec<Vec<bmv_trader::swing::Cand>>,
        usize,
        usize,
    ),
    String,
> {
    use bmv_trader::momentum::{self as mo, Calendar};
    use bmv_trader::scan::ADJUSTED_SIP;
    use bmv_trader::swing::{self as sw, Cand};
    let spy = bmv_trader::scan::read_daily(dir, ADJUSTED_SIP, mo::BENCHMARK)?;
    let cal = Calendar::new(&spy)?;
    let asset_names = if rules.stocks_only {
        bmv_trader::scan::read_asset_names(dir)?
    } else {
        BTreeMap::new()
    };
    let mut funds = 0;
    let mut names: Vec<String> = Vec::new();
    let mut days: Vec<Vec<Cand>> = vec![Vec::new(); cal.sessions.len()];
    bmv_trader::scan::for_each_daily(dir, ADJUSTED_SIP, &mut |s, bars| {
        if rules.stocks_only && asset_names.get(s).is_some_and(|n| sw::is_fund(n)) {
            funds += 1;
            return Ok(());
        }
        for (d, c) in sw::candidates(names.len(), &bars, &cal, rules) {
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
    let cut = first(cut_date).ok_or("no session at the holdout start")?;
    if rules.stocks_only {
        eprintln!("{funds} funds left out by name");
    }
    Ok((cal, names, days, start, cut))
}

/// Trial 12's sample (DESIGN §7i): each untouched-year session's top trial 9 signal, frozen once.
fn retro_sample() -> Result<(), String> {
    use bmv_trader::retro::{self, Pick};
    let (home, dir) = (config::home()?, data_dir()?);
    let _lock = retro::lock(&home)?;
    let cut_date = holdout_from(&dir, &[])?;
    if cut_date != retro::FROM {
        return Err(format!(
            "the holdout starts {cut_date}, but trial 12 is pre-registered from {}",
            retro::FROM
        ));
    }
    let (cal, names, days, _, cut) = swing_days(&dir, &bmv_trader::swing::TRIAL_9, &cut_date)?;
    // the hold (entry next session, up to MAX_HOLD more) must end inside the data
    let last = cal
        .sessions
        .len()
        .saturating_sub(bmv_trader::swing::MAX_HOLD + 1);
    let picks: Vec<Pick> = retro::top_per_session(&days, &names, cut..last)
        .into_iter()
        .map(|(d, c)| Pick {
            date: cal.sessions[d].clone(),
            symbol: names[c.symbol].clone(),
            ratio: c.ratio.unwrap_or(0.0),
        })
        .collect();
    let path = retro::freeze(&home, &picks)?;
    let hash = retro::sha256(&path)?;
    println!(
        "trial 12 sample frozen: {} signals, {}..{}; sha256 {hash}",
        picks.len(),
        picks.first().map_or("", |p| p.date.as_str()),
        picks.last().map_or("", |p| p.date.as_str())
    );
    Ok(())
}

/// Speed and load check (DESIGN §7i step 4): the local analyst on `count` research-period (2024)
/// signals. Outcomes are never computed; timings go to data/retro/speed.jsonl.
fn retro_speed(count: usize) -> Result<(), String> {
    use bmv_trader::desk::{self, Analyst, Status};
    use bmv_trader::retro;
    let (home, dir) = (config::home()?, data_dir()?);
    let _lock = retro::lock(&home)?;
    let cut_date = holdout_from(&dir, &[])?;
    let (cal, names, days, start, cut) = swing_days(&dir, &bmv_trader::swing::TRIAL_9, &cut_date)?;
    let in_2024: Vec<usize> = (start..cut)
        .filter(|&d| cal.sessions[d].starts_with("2024-"))
        .collect();
    let step = (in_2024.len() / count.max(1)).max(1);
    let picks = retro::top_per_session(&days, &names, start..cut);
    for &d in in_2024.iter().step_by(step).take(count) {
        let Some((_, c)) = picks.iter().find(|(x, _)| *x == d) else {
            continue;
        };
        let (symbol, date) = (&names[c.symbol], &cal.sessions[d]);
        let t = std::time::Instant::now();
        let (status, calls) = desk::analyse(&home, &Analyst::Local, symbol, date);
        let minutes = t.elapsed().as_secs_f64() / 60.0;
        let what = match &status {
            Status::Rated { rating, .. } => format!("rated {rating:?}"),
            Status::Failed { reason, .. } => format!("FAILED: {reason:?}"),
            other => format!("{other:?}"),
        };
        let (input, output): (u64, u64) = calls
            .iter()
            .fold((0, 0), |a, c| (a.0 + c.input, a.1 + c.output));
        println!(
            "{date} {symbol}: {what} in {minutes:.1} min, {} calls, {input} in / {output} out tokens",
            calls.len()
        );
        append_jsonl(
            &retro::dir(&home).join("speed.jsonl"),
            &[
                serde_json::json!({"date": date, "symbol": symbol, "minutes": minutes, "calls": calls.len(),
                "input": input, "output": output, "ok": matches!(status, Status::Rated { .. })}),
            ],
        )?;
    }
    Ok(())
}

/// Trial 12's runner: analyses the frozen sample in order until `hours` pass; resumes where it
/// stopped. Prints each answer; outcomes are never computed here.
fn retro_run(hours: usize) -> Result<(), String> {
    use bmv_trader::desk::{self, Analyst, Status};
    use bmv_trader::retro::{self, Row};
    let home = config::home()?;
    let _lock = retro::lock(&home)?;
    if logged_trials(&data_dir()?.join("trials.jsonl"))?
        .iter()
        .any(|t| t == retro::TRIAL_NAME)
    {
        return Err("trial 12 is already logged".into());
    }
    let picks = retro::read_sample(&home)?;
    bmv_trader::llm::ollama_has(desk::LOCAL_MODEL, std::time::Duration::from_secs(5))
        .map_err(|e| format!("the local analyst isn't ready: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(hours as u64 * 3600);
    let mut fails_in_a_row = 0;
    let mut done = retro::finals(&retro::read_rows(&home)?);
    let total = picks.len();
    for p in picks {
        if done.contains_key(&(p.date.clone(), p.symbol.clone())) {
            continue;
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "time is up: {} of {total} answered; run retro-run again to continue",
                done.len()
            );
            return Ok(());
        }
        let when = || et_now().format("%Y-%m-%dT%H:%M:%S").to_string();
        retro::append_row(
            &home,
            &Row {
                when: when(),
                pick: p.clone(),
                status: Status::Queued,
            },
        )?;
        let t = std::time::Instant::now();
        let (status, _) = desk::analyse(&home, &Analyst::Local, &p.symbol, &p.date);
        retro::append_row(
            &home,
            &Row {
                when: when(),
                pick: p.clone(),
                status: status.clone(),
            },
        )?;
        let what = match &status {
            Status::Rated { rating, .. } => format!("{rating:?}"),
            Status::Failed { reason, .. } => format!("failed: {reason:?}"),
            other => format!("{other:?}"),
        };
        fails_in_a_row = if matches!(status, Status::Failed { .. }) {
            fails_in_a_row + 1
        } else {
            0
        };
        done.insert((p.date.clone(), p.symbol.clone()), status);
        println!(
            "{}/{total} {} {}: {what} ({:.1} min)",
            done.len(),
            p.date,
            p.symbol,
            t.elapsed().as_secs_f64() / 60.0
        );
        if fails_in_a_row >= retro::MAX_FAILS_IN_A_ROW {
            return Err(format!(
                "{fails_in_a_row} failures in a row: stopping so an outage can't sweep the sample; \
                 check Ollama and data/desk/ta-logs, then run retro-run again"
            ));
        }
    }
    println!("all {total} signals answered: run retro-verdict");
    Ok(())
}

/// Trial 12 so far: counts and ratings only (no outcomes before the verdict).
fn retro_report() -> Result<(), String> {
    use bmv_trader::desk::Status;
    use bmv_trader::retro;
    let home = config::home()?;
    let picks = retro::read_sample(&home)?;
    let done = retro::finals(&retro::read_rows(&home)?);
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for s in done.values() {
        let k = match s {
            Status::Rated { rating, .. } => format!("{rating:?}"),
            Status::Failed { .. } => "failed".into(),
            other => format!("{other:?}"),
        };
        *by.entry(k).or_default() += 1;
    }
    println!(
        "trial 12: {} of {} signals answered",
        done.len(),
        picks.len()
    );
    for (k, n) in by {
        println!("  {k}: {n}");
    }
    Ok(())
}

/// Trial 12's verdict (DESIGN §7i): once, after every pick is answered; logged as trial 12.
fn retro_verdict() -> Result<(), String> {
    use bmv_trader::desk::{self, Scorecard, Status};
    use bmv_trader::retro;
    use bmv_trader::swing::Scored;
    let (home, dir) = (config::home()?, data_dir()?);
    let _lock = retro::lock(&home)?;
    let log = dir.join("trials.jsonl");
    if logged_trials(&log)?.iter().any(|t| t == retro::TRIAL_NAME) {
        return Err("trial 12 is already logged".into());
    }
    let picks = retro::read_sample(&home)?;
    let done = retro::finals(&retro::read_rows(&home)?);
    let open = picks
        .iter()
        .filter(|p| !done.contains_key(&(p.date.clone(), p.symbol.clone())))
        .count();
    if open > 0 {
        return Err(format!(
            "{open} of {} signals have no answer yet: run retro-run",
            picks.len()
        ));
    }
    let cut_date = holdout_from(&dir, &[])?;
    let (cal, names, days, _, _) = swing_days(&dir, &bmv_trader::swing::TRIAL_9, &cut_date)?;
    let mut cards: Vec<Scorecard> = Vec::new();
    let mut failed = 0;
    for p in &picks {
        let Some(Status::Rated { rating, .. }) = done.get(&(p.date.clone(), p.symbol.clone()))
        else {
            failed += 1;
            continue;
        };
        let d = cal
            .sessions
            .iter()
            .position(|s| *s == p.date)
            .ok_or_else(|| format!("{} not in the calendar", p.date))?;
        let c = days[d]
            .iter()
            .find(|c| names[c.symbol] == p.symbol)
            .ok_or_else(|| format!("{} {}: the signal is gone from the bars", p.date, p.symbol))?;
        cards.push(Scorecard {
            date: p.date.clone(),
            rank: 1,
            symbol: p.symbol.clone(),
            rating: *rating,
            scored: Scored::Done {
                r: c.out.r,
                exit_date: cal.sessions[c.out.exit].clone(),
            },
        });
    }
    let share = failed as f64 / picks.len() as f64;
    if share > retro::MAX_FAILED_SHARE {
        return Err(format!(
            "{failed} of {} signals failed ({:.0}%, over the pre-registered {:.0}%): trial 12 is void, not judged (DESIGN §7i)",
            picks.len(),
            share * 100.0,
            retro::MAX_FAILED_SHARE * 100.0
        ));
    }
    let gate = desk::verdict(&cards, retro::TRIAL_NUMBER);
    let t = desk::tally(&cards);
    let alpha = stats::FAMILY_ALPHA / retro::TRIAL_NUMBER as f64;
    let trial = stats::Trial {
        when: chrono::Utc::now().to_rfc3339(),
        strategy: retro::TRIAL_NAME.into(),
        params: format!(
            "{} rated, {failed} failed; {} both tiers, reasoning off; D in R; ci_low is D's one-sided bound at 1 - 0.05/12",
            cards.len(),
            desk::LOCAL_MODEL
        ),
        period: format!(
            "{}..{}",
            picks.first().map_or("", |p| p.date.as_str()),
            picks.last().map_or("", |p| p.date.as_str())
        ),
        trades: cards.len(),
        expectancy_r: t.weighted.unwrap_or(0.0),
        profit_factor: None,
        max_drawdown_r: 0.0,
        ci_low: desk::d_lower_bound(&cards, alpha, stats::resamples_for(alpha)),
    };
    stats::record_trial(&log, &trial)?;
    println!(
        "trial 12, {}: {} rated, {failed} failed",
        retro::TRIAL_NAME,
        cards.len()
    );
    println!(
        "  mean R: all {:?}, approved {:?}, vetoed {:?}, AI-weighted {:?}; D {:?}",
        t.mean_all, t.mean_approved, t.mean_vetoed, t.weighted, t.d
    );
    for c in &gate.checks {
        println!(
            "  [{}] {}: {}",
            if c.passed { "pass" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    println!(
        "{}",
        if gate.passed() {
            "TRIAL 12 PASSED"
        } else {
            "trial 12 failed"
        }
    );
    Ok(())
}

/// The intraday trials' research-period results, rerun exactly as logged (DESIGN §13d). Nothing is
/// logged; the holdout split is the same as `run_backtest`'s, so holdout days are never run.
fn intraday_research(
    from: &str,
    to: &str,
    symbols: &[String],
    o: &Opts,
    model: FillModel,
) -> Result<Vec<DayResult>, String> {
    let dir = data_dir()?;
    let days = if o.scan {
        bmv_trader::scan::load_scan_range(&dir, from, to)?
    } else {
        history::load_range(&dir, symbols, from, to)?
    };
    let dates: Vec<&str> = days.iter().map(|d| d.date.as_str()).collect();
    let cut_date = holdout_from(&dir, &dates)?;
    let cut = days.partition_point(|d| d.date < cut_date);
    let settings = Settings {
        vetting: false,
        orb: o.orb.clone(),
        entry: o.strategy.entry(),
        trend_filter: o.trend,
    };
    backtest::run(&days[..cut], settings, model)
}

/// The SPY regime of the last session before `date` (the signal day, never the trade's own).
fn regime_before(reg: &BTreeMap<String, bool>, date: &str) -> Option<bool> {
    reg.range(..date.to_string()).next_back().map(|(_, u)| *u)
}

/// What a trial rerun shares: the data, the hold-out cut, SPY's regime and the watchlist.
struct TrialCtx {
    dir: std::path::PathBuf,
    cut_date: String,
    reg: std::collections::BTreeMap<String, bool>,
    watch: Vec<String>,
}

/// One trial's reproduced results.
struct Rerun {
    rows: Vec<bmv_trader::autopsy::Row>,
    trades: usize,
    expectancy: f64,
    /// Before costs, where the trial has one (plain reruns only).
    gross: Option<f64>,
    units: &'static str,
}

fn trial_ctx() -> Result<TrialCtx, String> {
    use bmv_trader::momentum as mo;
    use bmv_trader::scan::ADJUSTED_SIP;
    let dir = data_dir()?;
    let cut_date = holdout_from(&dir, &[])?;
    let spy = bmv_trader::scan::read_daily(&dir, ADJUSTED_SIP, mo::BENCHMARK)?;
    let reg = bmv_trader::autopsy::spy_regime(&spy);
    let watch = WATCHLIST.split(',').map(str::to_string).collect();
    Ok(TrialCtx {
        dir,
        cut_date,
        reg,
        watch,
    })
}

fn no_params(orb: Option<OrbParams>) -> Result<(), String> {
    match orb {
        // shortcut: only trials 1-6 take parameters at run time; thread the swing and momentum constants through when one of them is worth a sensitivity run
        Some(_) => Err("this trial's parameters are compiled constants".into()),
        None => Ok(()),
    }
}

/// Reruns one finished trial from its own code path and options, at `cost` times its costs and,
/// for trials 1-6, with other ORB parameters.
fn rerun(
    trial: usize,
    t: &stats::Trial,
    c: &TrialCtx,
    cost: f64,
    orb: Option<OrbParams>,
) -> Result<Rerun, String> {
    use bmv_trader::autopsy::Row;
    use bmv_trader::momentum as mo;
    use bmv_trader::swing as sw;
    let (from, to) = t.period.split_once("..").ok_or("bad period")?;
    let opts = |strategy, trend, scan, buffer| Opts {
        fold_days: 21,
        seeds: 5,
        holdout: false,
        holdout_again: false,
        trend,
        scan,
        symbols_given: false,
        orb: OrbParams {
            entry_buffer_bps: buffer,
            ..OrbParams::default()
        },
        strategy,
    };
    let (rows, trades, expectancy, gross, units): (
        Vec<Row>,
        usize,
        f64,
        Option<f64>,
        &'static str,
    ) = match trial {
        1..=6 => {
            let mut o = match trial {
                1 => opts(Strategy::Orb, false, false, 0),
                2 => opts(Strategy::Orb, true, false, 0),
                3 => opts(Strategy::Orb, false, false, MARKETABLE_BUFFER_BPS),
                4 => opts(Strategy::Gap, false, false, 0),
                5 => opts(Strategy::Vwap, false, false, 0),
                _ => opts(Strategy::Gap, false, true, 0),
            };
            if let Some(p) = orb.clone() {
                o.orb = OrbParams {
                    entry_buffer_bps: o.orb.entry_buffer_bps,
                    ..p
                };
            }
            let base = FillModel::default();
            let model = FillModel {
                slippage_cents: (base.slippage_cents as f64 * cost).round() as i64,
                slippage_bps: (base.slippage_bps as f64 * cost).round() as i64,
                ..base
            };
            let res = intraday_research(from, to, &c.watch, &o, model)?;
            // the gross run only for the plain rerun the autopsy reads
            let g = if cost == 1.0 && orb.is_none() {
                let free = FillModel {
                    slippage_cents: 0,
                    slippage_bps: 0,
                    ..base
                };
                Some(
                    backtest::summarize(&intraday_research(from, to, &c.watch, &o, free)?)
                        .expectancy_r,
                )
            } else {
                None
            };
            let s = backtest::summarize(&res);
            let rows = res
                .iter()
                .flat_map(|d| &d.trades)
                .map(|tr| Row {
                    date: tr.date.clone(),
                    net: tr.r_multiple(),
                    exit: match tr.reason {
                        backtest::ExitReason::Stop => "stop",
                        backtest::ExitReason::Target => "target",
                        backtest::ExitReason::Flatten => "flatten",
                    },
                    hold: None,
                    up_market: regime_before(&c.reg, &tr.date),
                })
                .collect();
            (rows, s.trades, s.expectancy_r, g, "R")
        }
        7 => {
            no_params(orb)?;
            let (cal, by_symbol) = trial7_months(&c.dir, &c.cut_date)?;
            let st = mo::study_at_cost(&by_symbol, &cal, &c.cut_date, mo::COST_PER_SIDE * cost)?;
            let rows = st
                .dates
                .iter()
                .zip(&st.strategy)
                .map(|(d, r)| Row {
                    date: d.clone(),
                    net: *r,
                    exit: "month",
                    hold: None,
                    up_market: regime_before(&c.reg, d),
                })
                .collect();
            (
                rows,
                st.strategy.len() * mo::HOLD,
                mo::mean(&st.strategy),
                None,
                "monthly return",
            )
        }
        8 | 9 => {
            no_params(orb)?;
            let rules = if trial == 8 { sw::TRIAL_8 } else { sw::TRIAL_9 };
            let (cal, names, days, start, cut) = swing_days(&c.dir, &rules, &c.cut_date)?;
            let (taken, _) = sw::portfolio(&days, &names, start, cut);
            let cost_r = |o: &sw::Outcome| 2.0 * sw::COST_PER_SIDE / o.stop_fraction.max(1e-9);
            let rows: Vec<Row> = taken
                .iter()
                .map(|tk| {
                    let o = &tk.out;
                    let g = o.r + cost_r(o);
                    let date = cal.sessions[o.entry].clone();
                    Row {
                        up_market: regime_before(&c.reg, &date),
                        date,
                        // a cost shock adds (cost - 1) more of the round trip's cost
                        // shortcut: cost_r prices both sides at the entry price; rerun sw::portfolio at the shocked cost if a swing trial ever nears the bar
                        net: o.r - (cost - 1.0) * cost_r(o),
                        // shortcut: exit kind from the gross R (stop -1R, target +1.5R), approximate on gaps
                        exit: if o.at_data_end {
                            "data end"
                        } else if g >= sw::TARGET_ATR / sw::STOP_ATR - 0.01 {
                            "target"
                        } else if g <= -0.99 {
                            "stop"
                        } else {
                            "time"
                        },
                        hold: Some((o.exit - o.entry) as u32),
                    }
                })
                .collect();
            let gross = mo::mean(
                &taken
                    .iter()
                    .map(|tk| tk.out.r + cost_r(&tk.out))
                    .collect::<Vec<_>>(),
            );
            let e = mo::mean(&rows.iter().map(|r| r.net).collect::<Vec<_>>());
            (rows, taken.len(), e, Some(gross), "R")
        }
        11 => {
            use bmv_trader::lab;
            no_params(orb)?;
            if cost != 1.0 {
                return Err("trial 11 is a ranking score before costs".into());
            }
            let frozen: Vec<lab::Candidate> =
                std::fs::read_to_string(c.dir.join("lab/frozen.jsonl"))
                    .map_err(|e| format!("lab/frozen.jsonl: {e}"))?
                    .lines()
                    .map(serde_json::from_str)
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("lab/frozen.jsonl: {e}"))?;
            let (cal, panel) = lab_panel(&c.dir, lab::VALIDATION_END)?;
            let months = panel.range(&cal, lab::SEARCH_END, lab::VALIDATION_END);
            // the pick the trial logged: the best IC bound
            let best = frozen
                .iter()
                .filter_map(|c| {
                    let s = lab::evaluate(&c.expr, &panel, &months)?;
                    Some((c, lab::ic_bound(&s.ics, lab::TRIAL_11_NUMBER), s))
                })
                .max_by(|a, b| a.1.unwrap_or(f64::MIN).total_cmp(&b.1.unwrap_or(f64::MIN)))
                .ok_or("no frozen picks")?;
            let mut prev = Vec::new();
            let mut rows = Vec::new();
            for &k in &months {
                if let Some(m) = lab::score_month(&best.0.expr, &panel.months[k], &prev) {
                    if let Some(ic) = m.ic {
                        rows.push(Row {
                            date: panel.dates[k].clone(),
                            net: ic,
                            exit: "month",
                            hold: None,
                            up_market: regime_before(&c.reg, &panel.dates[k]),
                        });
                    }
                    prev = m.top;
                }
            }
            (
                rows,
                months.len() * lab::HOLD,
                best.2.ic_mean,
                None,
                "monthly IC",
            )
        }
        other => {
            return Err(format!(
                "trial {other} has no autopsy (10 and 12 wait for their verdicts)"
            ));
        }
    };
    Ok(Rerun {
        rows,
        trades,
        expectancy,
        gross,
        units,
    })
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fingerprint of the market data the trials read: every file's path and size, hashed. A
/// re-downloaded or edited file changes it.
// shortcut: sizes, not contents (64k files); hash contents if a same-size edit ever matters
fn data_fingerprint(dir: &Path) -> Result<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<String>) -> Result<(), String> {
        let mut entries: Vec<_> = std::fs::read_dir(at)
            .map_err(|e| format!("{}: {e}", at.display()))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{}: {e}", at.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let meta = e.metadata().map_err(|err| err.to_string())?;
            if meta.is_dir() {
                walk(root, &e.path(), out)?;
            } else {
                let rel = e.path();
                let rel = rel.strip_prefix(root).unwrap_or(&rel).display().to_string();
                out.push(format!("{rel}\t{}", meta.len()));
            }
        }
        Ok(())
    }
    let mut lines = Vec::new();
    for sub in ["history", "daily", "scan", "lab", "holdout_from"] {
        let p = dir.join(sub);
        if p.is_dir() {
            walk(dir, &p, &mut lines)?;
        } else if let Ok(m) = std::fs::metadata(&p) {
            lines.push(format!("{sub}\t{}", m.len()));
        }
    }
    Ok(sha256_hex(&lines.join("\n")))
}

/// The results of a run, as a hash: every row's date, exact value and exit.
fn results_hash(rows: &[bmv_trader::autopsy::Row]) -> String {
    let text: Vec<String> = rows
        .iter()
        .map(|r| format!("{}|{:016x}|{}", r.date, r.net.to_bits(), r.exit))
        .collect();
    sha256_hex(&text.join("\n"))
}

/// Each ORB number at 80% and 120%, one at a time (integers rounded, never below 1).
fn orb_neighbours(p: &OrbParams) -> Vec<(String, OrbParams)> {
    let i = |v: i64, f: f64| ((v as f64 * f).round() as i64).max(1);
    let u = |v: usize, f: f64| ((v as f64 * f).round() as usize).max(1);
    let mut out = Vec::new();
    for f in [0.8, 1.2] {
        let tag = |name: &str| format!("{name} x{f}");
        out.push((
            tag("or_minutes"),
            OrbParams {
                or_minutes: i(p.or_minutes, f),
                ..p.clone()
            },
        ));
        out.push((
            tag("min_or_bars"),
            OrbParams {
                min_or_bars: u(p.min_or_bars, f),
                ..p.clone()
            },
        ));
        out.push((
            tag("atr_len"),
            OrbParams {
                atr_len: u(p.atr_len, f),
                ..p.clone()
            },
        ));
        out.push((
            tag("atr_mult"),
            OrbParams {
                atr_mult: p.atr_mult * f,
                ..p.clone()
            },
        ));
        out.push((
            tag("target_r"),
            OrbParams {
                target_r: p.target_r * f,
                ..p.clone()
            },
        ));
        out.push((
            tag("min_r_frac"),
            OrbParams {
                min_r_frac: p.min_r_frac * f,
                ..p.clone()
            },
        ));
        out.push((
            tag("min_r_cents"),
            OrbParams {
                min_r_cents: i(p.min_r_cents, f),
                ..p.clone()
            },
        ));
    }
    out
}

/// `bmv-trader stress-battery` (build step 3, DESIGN §4c): every finished trial reproduced twice,
/// with input and result hashes, then put through the owner's stress tests and judged on the bars
/// in `stress.rs`. Written to data/stress.jsonl. Nothing here trades or calls a model.
fn stress_battery() -> Result<(), String> {
    use bmv_trader::stress::{self, Check};
    let (home, dir) = (config::home()?, data_dir()?);
    let logged: Vec<stats::Trial> = std::fs::read_to_string(dir.join("trials.jsonl"))
        .map_err(|e| format!("trials.jsonl: {e}"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("trials.jsonl: {e}"))?;
    let curated: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(home.join("docs/lessons.json"))
            .map_err(|e| format!("docs/lessons.json: {e}"))?,
    )
    .map_err(|e| format!("docs/lessons.json: {e}"))?;
    let ctx = trial_ctx()?;
    let data = data_fingerprint(&dir)?;
    eprintln!("data fingerprint {}", &data[..16]);

    struct Done {
        trial: usize,
        name: String,
        units: &'static str,
        net: Vec<f64>,
        value: serde_json::Value,
        checks: Vec<Check>,
    }
    let mut done = Vec::new();
    for c in &curated {
        let trial = c["trial"].as_u64().unwrap_or(0) as usize;
        let row = c["log_row"].as_u64().unwrap_or(0) as usize;
        let t = logged
            .get(row.wrapping_sub(1))
            .ok_or_else(|| format!("trial {trial}: no row {row} in trials.jsonl"))?;
        eprintln!("trial {trial} ({}): reproducing twice", t.strategy);
        let a = rerun(trial, t, &ctx, 1.0, None)?;
        let b = rerun(trial, t, &ctx, 1.0, None)?;
        let inputs = sha256_hex(
            &serde_json::json!({"trial": trial, "strategy": t.strategy, "params": t.params,
                                "period": t.period, "cost": 1.0, "data": data})
            .to_string(),
        );
        let (ha, hb) = (results_hash(&a.rows), results_hash(&b.rows));
        let matches_log = a.trades == t.trades && (a.expectancy - t.expectancy_r).abs() < 1e-9;
        let reproduces = ha == hb && matches_log;
        let net: Vec<f64> = a.rows.iter().map(|r| r.net).collect();
        let mut checks = vec![Check {
            test: "reproduces",
            passed: Some(reproduces),
            detail: if reproduces {
                format!(
                    "two runs, same results hash; matches the log ({} trades)",
                    t.trades
                )
            } else if ha != hb {
                "two runs gave different results".into()
            } else {
                format!(
                    "logged {} trades {:+.4}, rerun {} trades {:+.4}",
                    t.trades, t.expectancy_r, a.trades, a.expectancy
                )
            },
        }];

        let mc = stress::monte_carlo(&net, stress::MC_DRAWS, stress::MC_SEED);
        checks.push(Check {
            test: "monte carlo",
            passed: mc.as_ref().map(|m| m.p5_mean > stress::MC_P5_MIN),
            detail: mc.as_ref().map_or("no results".into(), |m| {
                format!(
                    "5th percentile {:+.4} {}, {:.0}% of resamples below 0",
                    m.p5_mean,
                    a.units,
                    m.p_negative * 100.0
                )
            }),
        });

        eprintln!("trial {trial}: cost shocks");
        let mut shocks = Vec::new();
        for k in [2.0, 3.0] {
            shocks.push((k, rerun(trial, t, &ctx, k, None).map(|r| r.expectancy)));
        }
        let at_double = shocks
            .iter()
            .find(|s| s.0 == stress::COST_SHOCK)
            .map(|s| &s.1);
        checks.push(Check {
            test: "cost x2",
            passed: at_double.and_then(|r| r.as_ref().ok()).map(|e| *e > 0.0),
            detail: shocks
                .iter()
                .map(|(k, r)| match r {
                    Ok(e) => format!("x{k}: {e:+.4}"),
                    Err(e) => format!("x{k}: not run ({e})"),
                })
                .collect::<Vec<_>>()
                .join(", "),
        });

        let neighbours: Vec<(String, Result<f64, String>)> = if (1..=6).contains(&trial) {
            eprintln!("trial {trial}: 14 parameter neighbours");
            orb_neighbours(&OrbParams::default())
                .into_iter()
                .map(|(name, p)| {
                    (
                        name,
                        rerun(trial, t, &ctx, 1.0, Some(p)).map(|r| r.expectancy),
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let ran: Vec<f64> = neighbours
            .iter()
            .filter_map(|n| n.1.as_ref().ok().copied())
            .collect();
        let same = ran
            .iter()
            .filter(|e| e.signum() == a.expectancy.signum())
            .count();
        checks.push(Check {
            test: "parameters ±20%",
            passed: (!ran.is_empty() && ran.len() == neighbours.len())
                .then(|| same as f64 / ran.len() as f64 >= stress::NEIGHBOUR_SHARE),
            detail: if neighbours.is_empty() {
                "not tested: this trial's parameters are compiled constants".into()
            } else {
                format!("{same} of {} neighbours keep the sign", neighbours.len())
            },
        });

        let years = stress::by_year(&a.rows);
        let positive = years.iter().filter(|y| y.mean > 0.0).count();
        checks.push(Check {
            test: "by year",
            passed: (!years.is_empty())
                .then(|| positive as f64 / years.len() as f64 >= stress::YEAR_SHARE),
            detail: format!("{positive} of {} years positive", years.len()),
        });
        let regimes = stress::by_regime(&a.rows);

        let value = serde_json::json!({
            "trial": trial,
            "name": t.strategy,
            "period": t.period,
            "units": a.units,
            "inputs_hash": inputs,
            "results_hash": ha,
            "trades": a.trades,
            "expectancy": a.expectancy,
            "monte_carlo": mc,
            "cost_shocks": shocks.iter().map(|(k, r)| serde_json::json!({"x": k, "expectancy": r.as_ref().ok(), "error": r.as_ref().err()})).collect::<Vec<_>>(),
            "neighbours": neighbours.iter().map(|(n, r)| serde_json::json!({"change": n, "expectancy": r.as_ref().ok(), "error": r.as_ref().err()})).collect::<Vec<_>>(),
            "years": years,
            "regimes": regimes,
        });
        done.push(Done {
            trial,
            name: t.strategy.clone(),
            units: a.units,
            net,
            value,
            checks,
        });
    }

    // deflated Sharpe: each trial against the best of every registered trial by luck alone
    let sharpes: Vec<f64> = done
        .iter()
        .filter_map(|d| stress::moments(&d.net).map(|m| m.0))
        .collect();
    let mean_sr = sharpes.iter().sum::<f64>() / sharpes.len().max(1) as f64;
    let var_sr = sharpes.iter().map(|s| (s - mean_sr).powi(2)).sum::<f64>()
        / (sharpes.len().max(2) - 1) as f64;
    // trial 12 is registered too, though its verdict waits
    let tried = logged
        .len()
        .max(done.iter().map(|d| d.trial).max().unwrap_or(0))
        + 1;
    let mut out = Vec::new();
    println!("trial  name                            verdict  tests");
    for mut d in done {
        let dsr = stress::moments(&d.net).map(|(sr, skew, kurt)| {
            stress::deflated_sharpe(sr, d.net.len(), skew, kurt, tried, var_sr)
        });
        d.checks.push(Check {
            test: "deflated sharpe",
            passed: dsr.map(|p| p > stress::DSR_MIN),
            detail: dsr.map_or("too few results".into(), |p| {
                format!("{p:.3} against {tried} trials (bar {})", stress::DSR_MIN)
            }),
        });
        let pass = stress::verdict(&d.checks);
        let marks: Vec<String> = d
            .checks
            .iter()
            .map(|c| {
                format!(
                    "{} {}",
                    match c.passed {
                        Some(true) => "✓",
                        Some(false) => "✗",
                        None => "–",
                    },
                    c.test
                )
            })
            .collect();
        println!(
            "{:>5}  {:<30}  {:<7}  {}",
            d.trial,
            d.name,
            if pass { "PASS" } else { "FAIL" },
            marks.join(", ")
        );
        for c in &d.checks {
            println!("         {}: {}", c.test, c.detail);
        }
        let mut v = d.value;
        v["units"] = d.units.into();
        v["checks"] = serde_json::to_value(&d.checks).map_err(|e| e.to_string())?;
        v["verdict"] = if pass { "PASS" } else { "FAIL" }.into();
        out.push(v);
    }
    let path = dir.join("stress.jsonl");
    let tmp = dir.join("stress.jsonl.tmp");
    let text: String = out.iter().map(|v| format!("{v}\n")).collect();
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// `bmv-trader trial-autopsy` (DESIGN §13d, DECISION S27): every finished trial, reproduced from its
/// own code path and options, then broken down; written to data/lessons.jsonl.
fn trial_autopsy() -> Result<(), String> {
    use bmv_trader::autopsy;
    let (home, dir) = (config::home()?, data_dir()?);
    let logged: Vec<stats::Trial> = std::fs::read_to_string(dir.join("trials.jsonl"))
        .map_err(|e| format!("trials.jsonl: {e}"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("trials.jsonl: {e}"))?;
    let curated: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(home.join("docs/lessons.json"))
            .map_err(|e| format!("docs/lessons.json: {e}"))?,
    )
    .map_err(|e| format!("docs/lessons.json: {e}"))?;
    let ctx = trial_ctx()?;
    // (trial, log row, units)
    let mut out = Vec::new();
    for c in &curated {
        let trial = c["trial"].as_u64().unwrap_or(0) as usize;
        let row = c["log_row"].as_u64().unwrap_or(0) as usize;
        let Some(t) = logged.get(row.wrapping_sub(1)) else {
            return Err(format!("trial {trial}: no row {row} in trials.jsonl"));
        };
        eprintln!("trial {trial} ({}): rerunning {}", t.strategy, t.period);
        let Rerun {
            rows,
            trades,
            expectancy,
            gross,
            units,
        } = rerun(trial, t, &ctx, 1.0, None)?;
        let reproduces = trades == t.trades && (expectancy - t.expectancy_r).abs() < 1e-9;
        let a = autopsy::autopsy(&rows, gross);
        println!(
            "trial {trial:>2} {:<30} {} trades, {:+.3} {units}: {}",
            t.strategy,
            trades,
            expectancy,
            if reproduces {
                "reproduces ✓".to_string()
            } else {
                format!(
                    "DOES NOT REPRODUCE (logged {} trades, {:+.3})",
                    t.trades, t.expectancy_r
                )
            }
        );
        if reproduces {
            for f in &a.flags {
                println!("      flag: {f}");
            }
        }
        out.push(serde_json::json!({
            "trial": trial,
            "log_row": row,
            "name": t.strategy,
            "period": t.period,
            "units": units,
            "reproduces": reproduces,
            "logged_trades": t.trades,
            "logged_expectancy": t.expectancy_r,
            "autopsy": if reproduces { serde_json::to_value(&a).map_err(|e| e.to_string())? } else { serde_json::Value::Null },
            "lesson": c["lesson"],
            "next": c["next"],
            "acted_on_by": c["acted_on_by"],
        }));
    }
    let path = dir.join("lessons.jsonl");
    let tmp = dir.join("lessons.jsonl.tmp");
    let text: String = out.iter().map(|v| format!("{v}\n")).collect();
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {} trials to {}", out.len(), path.display());
    Ok(())
}

/// Trial 12's power gate (DESIGN §7i): research-period top signals only, never the untouched year.
fn retro_power() -> Result<(), String> {
    use bmv_trader::retro;
    let dir = data_dir()?;
    let cut_date = holdout_from(&dir, &[])?;
    if cut_date != retro::FROM {
        return Err(format!(
            "the holdout starts {cut_date}, but trial 12 is pre-registered from {}",
            retro::FROM
        ));
    }
    let (cal, names, days, start, cut) = swing_days(&dir, &bmv_trader::swing::TRIAL_9, &cut_date)?;
    let top = retro::top_per_session(&days, &names, start..cut);
    let rs: Vec<f64> = top.iter().map(|(_, c)| c.out.r).collect();
    let share = top.len() as f64 / (cut - start) as f64;
    // the untouched year's size from the calendar only: its sessions, less the last 10 (holds)
    let sessions = cal
        .sessions
        .len()
        .saturating_sub(cut + bmv_trader::swing::MAX_HOLD);
    let n = (share * sessions as f64).round() as usize;
    println!(
        "research {}..{}: {} sessions, {} with a signal ({:.0}%); top-signal R mean {:+.3}, sd {:.3}",
        cal.sessions[start],
        cal.sessions[cut - 1],
        cut - start,
        top.len(),
        share * 100.0,
        bmv_trader::momentum::mean(&rs),
        bmv_trader::power::sd(&rs)
    );
    println!("untouched year: {sessions} sessions with a full hold, so about {n} signals expected");
    let registered = [(0.5, 5.0), (0.0, 1.0)];
    let mixes: [(&str, &[(f64, f64)]); 3] = [
        (
            "trial 10 night 1: 5 Hold, 1 Underweight (the registered guide)",
            &registered,
        ),
        (
            "decisive: 40% Buy/Overweight, 30% Hold, 30% veto (for context)",
            &[(1.0, 0.4), (0.5, 0.3), (0.0, 0.3)],
        ),
        (
            "half Hold, a quarter each way (for context)",
            &[(1.0, 0.25), (0.5, 0.5), (0.0, 0.25)],
        ),
    ];
    println!("\n  MDE of D at 1 - 0.05/12, one-sided, {n} signals:");
    let mut gate = None;
    for (name, mix) in mixes {
        let sd = retro::d_null_sd(&rs, mix, n.max(10), 4000, 7);
        let (m50, m80) = (retro::mde(sd, 0.5), retro::mde(sd, 0.8));
        println!("  {m50:>6.3}R at 50%, {m80:>6.3}R at 80%   {name}");
        gate.get_or_insert(m80);
    }
    let m80 = gate.unwrap_or(f64::INFINITY);
    if m80 <= retro::MAX_MDE_R {
        println!(
            "\nGATE PASSED: {m80:.3}R <= {:.2}R, so trial 12 may be built (DECISION R11)",
            retro::MAX_MDE_R
        );
    } else {
        println!(
            "\nGATE FAILED: {m80:.3}R > {:.2}R, so trial 12 stops here (DECISION R9, R11)",
            retro::MAX_MDE_R
        );
    }
    Ok(())
}

fn backtest_swing(rules: bmv_trader::swing::Rules) -> Result<(), String> {
    use bmv_trader::momentum as mo;
    use bmv_trader::scan::ADJUSTED_SIP;
    use bmv_trader::swing as sw;
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
    let (cal, names, days, start, cut) = swing_days(&dir, &rules, &cut_date)?;
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

/// Trial 10's symbols: trial 9's universe (stocks by name) that still traded in the last 30
/// sessions of the research data, plus SPY for the calendar. Built once and cached in data/desk/,
/// so the universe is fixed for the whole test (DESIGN §7f).
fn live_symbols(dir: &Path) -> Result<Vec<String>, String> {
    use bmv_trader::scan::{self, ADJUSTED_SIP};
    let cache = dir.join("desk").join("live_symbols.json");
    if cache.exists() {
        return history::read_json(&cache);
    }
    let spy = scan::read_daily(dir, ADJUSTED_SIP, "SPY")?;
    let cutoff = spy
        .len()
        .checked_sub(30)
        .map(|i| spy[i].date.clone())
        .ok_or("too few SPY bars")?;
    let names = scan::read_asset_names(dir)?;
    let mut out = vec!["SPY".to_string()];
    for s in scan::read_universe(dir)? {
        if s == "SPY" || names.get(&s).is_some_and(|n| bmv_trader::swing::is_fund(n)) {
            continue;
        }
        let bars = scan::read_daily(dir, ADJUSTED_SIP, &s)?;
        if bars.last().is_some_and(|b| b.date >= cutoff) {
            out.push(s);
        }
    }
    history::write_json(&cache, &out)?;
    Ok(out)
}

/// Days of daily bars each night: enough that EMA 200 has forgotten its seed (DESIGN §7f).
const LIVE_DAYS: u64 = 1100;

fn et_now() -> chrono::DateTime<chrono_tz::Tz> {
    chrono::Utc::now().with_timezone(&chrono_tz::America::New_York)
}

fn live_calendar(dir: &Path) -> Result<bmv_trader::momentum::Calendar, String> {
    let spy = bmv_trader::scan::read_daily(dir, bmv_trader::scan::ADJUSTED_SIP_LIVE, "SPY")?;
    bmv_trader::momentum::Calendar::new(&spy)
}

/// Trial 10 nightly (DESIGN §7f). Shadow only: it reads Alpaca's market data and never orders.
/// Every signal is logged as Queued before any money is spent; a night cut short resumes there.
fn desk_run() -> Result<(), String> {
    use bmv_trader::desk::{self, Analysis, Status};
    let (home, dir) = (config::home()?, data_dir()?);
    let lock = desk::lock(&home)?;
    let et = et_now();
    let today = et.date_naive().to_string();
    let alpaca = connect()?;
    // Alpaca's calendar knows holidays and half-days
    let rows: Vec<history::CalendarRow> = alpaca.get(
        broker::Api::Trading,
        &format!("/v2/calendar?start={today}&end={today}"),
    )?;
    let session_today = history::parse_calendar(&rows)?
        .into_iter()
        .find(|s| s.date == today);
    if session_today
        .as_ref()
        .is_some_and(|s| et.timestamp() < s.close + 20 * 60)
    {
        return Err("run 20 minutes or more after today's close, so the daily bar is final".into());
    }
    let symbols = live_symbols(&dir)?;
    let from = (et.date_naive() - chrono::Days::new(LIVE_DAYS)).to_string();
    // the free plan refuses SIP data from the last 15 minutes; today's bar is stamped at midnight
    let end = (chrono::Utc::now() - chrono::Duration::minutes(20))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    eprintln!("refreshing {} symbols, {from}..{end}", symbols.len());
    bmv_trader::scan::refresh_live(&alpaca, &dir, &symbols, &from, &end, &mut |d, n| {
        if d % 1000 == 0 || d == n {
            eprintln!("  {d}/{n}");
        }
    })?;
    let cal = live_calendar(&dir)?;
    let target = cal.sessions.last().cloned().ok_or("no SPY bars")?;
    if session_today.is_some() && target != today {
        return Err(format!(
            "today is a session but SPY's newest bar is {target}: the data isn't ready, run again later"
        ));
    }
    let path = desk::signals_path(&home, &target);
    let signals: Vec<bmv_trader::swing::Signal> = if path.exists() {
        history::read_json(&path)?
    } else {
        let found = desk::find_signals(&home, &cal)?;
        history::write_json(&path, &found)?;
        let when = et_now().to_rfc3339();
        for (i, sig) in found.iter().enumerate() {
            desk::record(
                &home,
                &Analysis {
                    when: when.clone(),
                    signal: sig.clone(),
                    rank: i + 1,
                    status: Status::Queued,
                },
            )?;
        }
        found
    };
    println!("{} signals on {target}", signals.len());
    let key = desk::anthropic_key()?;
    for (i, sig) in signals.into_iter().enumerate() {
        let rank = i + 1;
        let current = desk::read_analyses(&home)?;
        let done = current.iter().any(|a| {
            a.signal.date == sig.date && a.signal.symbol == sig.symbol && a.status != Status::Queued
        });
        if done {
            continue;
        }
        let status = if rank > desk::MAX_PER_DAY {
            Status::Unanalysed {
                reason: format!("over the daily cap of {}", desk::MAX_PER_DAY),
            }
        } else if let Some(reason) = desk::over_budget(&desk::all_runs(&home)?, &today) {
            Status::Unanalysed { reason }
        } else {
            match &key {
                None => Status::Unanalysed {
                    reason: "no ANTHROPIC_API_KEY in the Keychain".into(),
                },
                Some(k) => {
                    eprintln!(
                        "  analysing #{rank} {} (volume ratio {:.1})",
                        sig.symbol, sig.ratio
                    );
                    desk::analyse(&home, &desk::Analyst::Claude(k), &sig.symbol, &sig.date).0
                }
            }
        };
        println!("  #{rank} {}: {}", sig.symbol, describe(&status));
        desk::record(
            &home,
            &Analysis {
                when: et_now().to_rfc3339(),
                signal: sig,
                rank,
                status,
            },
        )?;
    }
    let cards = desk::score_all(&home, &cal, &desk::read_analyses(&home)?)?;
    history::write_json(&desk::desk_dir(&home).join("outcomes.json"), &cards)?;
    desk_report(&home, &cal)?;
    // Tier 2 notes are best-effort: they never fail the night, and run without the desk lock
    drop(lock);
    if let Err(e) = notes() {
        eprintln!("notes: {e}");
    }
    Ok(())
}

/// Tier 2: writes the notes that are due (DESIGN §10). Each is written once.
fn notes() -> Result<(), String> {
    use bmv_trader::notes::{self, Outcome};
    use chrono::Datelike;
    let home = config::home()?;
    // the desk lock keeps a still-running desk-run's night from being written up half done
    let _desk = bmv_trader::desk::lock(&home)?;
    let _lock = notes::lock(&home)?;
    let et = et_now();
    let sunday = et.weekday() == chrono::Weekday::Sun;
    let mut failed = 0;
    for (name, out) in notes::run_due(&home, &et.date_naive().to_string(), sunday) {
        match out {
            Outcome::Written(p) => println!("  note {name}: {}", p.display()),
            Outcome::Fallback(p, e) => {
                println!(
                    "  note {name}: facts only, Tier 2 down ({e}): {}",
                    p.display()
                )
            }
            Outcome::Skipped(why) => println!("  note {name}: skipped ({why})"),
            Outcome::Failed(e) => {
                failed += 1;
                println!("  note {name}: FAILED ({e})")
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} note job(s) failed"));
    }
    Ok(())
}

fn rotated_path(home: &Path) -> std::path::PathBuf {
    home.join("data").join("llm_rotated.json")
}

/// The last rotation's error, kept until a rotation succeeds, so health reports it.
fn rotate_error_path(home: &Path) -> std::path::PathBuf {
    home.join("data").join("llm_rotate_error.json")
}

/// The free cloud models' key lifecycle (S42): with no argument, one OpenRouter pass; with
/// `--record PROVIDER`, logs a pasted key; with `--connection PROVIDER`, prints that provider's
/// OmniRoute connection and whether OmniRoute has it yet (for scripts/free-key-add.sh).
fn free_keys(record: Option<&str>, connection: Option<&str>) -> Result<(), String> {
    use bmv_trader::free_keys::{self, Config, KeyApi};
    let home = config::home()?;
    let cfg = Config::load(&home)?;
    let log = free_keys::log_path(&home);
    if let Some(p) = record {
        println!(
            "{}",
            free_keys::record_pasted(&cfg, &log, p, chrono::Utc::now())?
        );
        return Ok(());
    }
    let user_home = std::path::PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set")?);
    if let Some(p) = connection {
        let conn = free_keys::pasted_connection(&cfg, p)?;
        let listed = bmv_trader::llm::run_cli(&bmv_trader::llm::cli_call(
            &user_home,
            &["providers", "list", "--output", "json"],
        ))?;
        let exists = free_keys::listed_has(&listed, &conn)?;
        println!("{conn} {}", if exists { "existing" } else { "new" });
        return Ok(());
    }
    let (http, mgmt) = free_keys::openrouter_api()?;
    let create = |name: &str, limit: f64, expires: &str| {
        free_keys::openrouter_create(&http, &mgmt, name, limit, expires)
    };
    let delete = |hash: &str| free_keys::openrouter_delete(&http, &mgmt, hash);
    let list = || free_keys::openrouter_list(&http, &mgmt);
    // the key reaches OmniRoute in this one child's environment, never its arguments; `rotate`
    // runs OmniRoute's own connectivity test before it reports success
    let handover = |connection: &str, value: &str| {
        let mut c = bmv_trader::llm::cli_call(
            &user_home,
            &[
                "providers",
                "rotate",
                connection,
                "--from-env",
                free_keys::NEW_KEY_VAR,
                "--yes",
            ],
        );
        c.env.push((free_keys::NEW_KEY_VAR, value.to_string()));
        bmv_trader::llm::run_cli(&c).map(|_| ())
    };
    let test = |connection: &str| {
        bmv_trader::llm::run_cli(&bmv_trader::llm::cli_call(
            &user_home,
            &["providers", "test", connection],
        ))
        .map(|_| ())
    };
    let api = KeyApi {
        create: &create,
        delete: &delete,
        list: &list,
    };
    let omni = free_keys::OmniRoute {
        handover: &handover,
        test: &test,
    };
    let done = free_keys::run_openrouter(&cfg, &log, chrono::Utc::now(), &api, &omni)?;
    println!("free keys: {done}");
    Ok(())
}

/// Daily OmniRoute key rotation (DECISION S22). No Tier-2 call can be in flight: it holds the
/// desk and notes locks.
fn llm_rotate() -> Result<(), String> {
    let home = config::home()?;
    let _desk = bmv_trader::desk::lock(&home)?;
    let _notes = bmv_trader::notes::lock(&home)?;
    let user_home = std::path::PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set")?);
    match bmv_trader::llm::rotate(&user_home) {
        Ok(done) => {
            let _ = std::fs::remove_file(rotate_error_path(&home));
            history::write_json(&rotated_path(&home), &chrono::Utc::now().to_rfc3339())?;
            println!("{done}");
            Ok(())
        }
        Err(e) => {
            let when = chrono::Utc::now().to_rfc3339();
            if let Err(w) = history::write_json(&rotate_error_path(&home), &(when, e.clone())) {
                eprintln!("cannot record the rotation error: {w}");
            }
            Err(e)
        }
    }
}

/// Tier 1 and Tier 2 health, always written to data/llm_health.json (a check that can't run
/// is a failing check). Returns whether all checks pass.
/// The older of the two Claude keys' rotation dates, which scripts/rotate-keys.sh records per key
/// (S41): `{"engine": RFC 3339, "desk": RFC 3339}`.
fn keys_age(path: &std::path::Path) -> Result<String, String> {
    if !path.exists() {
        return Err("never recorded: run scripts/rotate-keys.sh".into());
    }
    let dates: std::collections::BTreeMap<String, String> = history::read_json(path)?;
    let mut oldest = i64::MIN;
    for key in ["engine", "desk"] {
        let t = dates.get(key).ok_or_else(|| {
            format!("the {key} key was never rotated: run scripts/rotate-keys.sh")
        })?;
        let t = chrono::DateTime::parse_from_rfc3339(t)
            .map_err(|e| format!("keys_rotated.json, {key}: {e}"))?;
        let days = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_days();
        if days < 0 {
            return Err(format!("keys_rotated.json, {key}: a date in the future"));
        }
        oldest = oldest.max(days);
    }
    if oldest <= 35 {
        Ok(format!("{oldest} days ago"))
    } else {
        Err(format!("{oldest} days ago: run scripts/rotate-keys.sh"))
    }
}

fn llm_health() -> Result<bool, String> {
    use bmv_trader::llm::{self, Check, Health};
    let home = config::home()?;
    // don't probe in the middle of a rotation or a notes run
    let mut checks = match bmv_trader::notes::lock(&home) {
        Ok(_lock) => llm::probe_checks(),
        Err(e) => vec![Check::of("Tier-2 probe", Err(format!("not run: {e}")))],
    };
    let age = match history::read_json::<String>(&rotated_path(&home)) {
        Ok(t) => chrono::DateTime::parse_from_rfc3339(&t)
            .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_hours())
            .map_err(|e| format!("llm_rotated.json: {e}")),
        Err(_) => Err("never rotated".into()),
    };
    checks.push(Check::of(
        "OmniRoute key rotated within 2 days",
        age.and_then(|h| {
            if (0..48).contains(&h) {
                Ok(format!("{h} h ago"))
            } else {
                Err(format!("{h} h ago"))
            }
        }),
    ));
    checks.push(Check::of(
        "last rotation succeeded",
        match history::read_json::<(String, String)>(&rotate_error_path(&home)) {
            Ok((when, e)) => Err(format!("{when}: {e}")),
            Err(_) => Ok("no recorded failure".into()),
        },
    ));
    let today = et_now().date_naive().to_string();
    checks.push(Check::of(
        "Claude spend (trial 10 and Tier 2) under the monthly cap",
        bmv_trader::desk::all_runs(&home).and_then(|runs| {
            let (_, trial) = bmv_trader::desk::spent(&runs, &today);
            let tier2 = llm::month_spend(&llm::spend_path(&home), None)?;
            let month = trial + tier2;
            let line = format!(
                "${month:.2} of ${:.0} this month (trial 10 ${trial:.2}, Tier 2 ${tier2:.2})",
                bmv_trader::desk::MONTHLY_BUDGET_USD
            );
            if month < bmv_trader::desk::MONTHLY_BUDGET_USD {
                Ok(line)
            } else {
                Err(line)
            }
        }),
    ));
    match bmv_trader::free_keys::Config::load(&home) {
        Ok(cfg) => {
            let events = bmv_trader::free_keys::read_log(&bmv_trader::free_keys::log_path(&home));
            match events {
                Ok(ev) => {
                    for (label, line) in
                        bmv_trader::free_keys::health(&cfg, &ev, chrono::Utc::now())
                    {
                        checks.push(Check::of(&label, line));
                    }
                }
                Err(e) => checks.push(Check::of("free keys log", Err(e))),
            }
        }
        Err(e) => checks.push(Check::of("free keys config", Err(e))),
    }
    checks.push(Check::of(
        "Claude keys rotated within 35 days",
        keys_age(&home.join("data").join("keys_rotated.json")),
    ));
    let h = Health {
        when: chrono::Utc::now().to_rfc3339(),
        checks,
    };
    history::write_json(&llm::health_path(&home), &h)?;
    for c in &h.checks {
        println!(
            "  [{}] {}: {}",
            if c.ok { "ok" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    Ok(h.ok())
}

fn describe(s: &bmv_trader::desk::Status) -> String {
    use bmv_trader::desk::Status;
    match s {
        Status::Queued => "queued".into(),
        Status::Rated {
            rating, cost_usd, ..
        } => format!("{rating:?} (${cost_usd:.2})"),
        Status::Failed { reason, cost_usd } => format!("failed: {reason} (${cost_usd:.2})"),
        Status::Unanalysed { reason } => format!("not analysed: {reason}"),
    }
}

/// Trial 10 so far: counts, spend, missed sessions and when the verdict is due. Outcome averages
/// stay hidden until the verdict, so nobody can stop the test on a good-looking night.
fn desk_report(
    home: &Path,
    cal: &bmv_trader::momentum::Calendar,
) -> Result<Vec<bmv_trader::desk::Scorecard>, String> {
    use bmv_trader::desk;
    let p = desk::progress(home, cal, &et_now().date_naive().to_string())?;
    println!(
        "trial 10: {} signals, {} rated, {} failed, {} not analysed, {} queued",
        p.signals, p.rated, p.failed, p.unanalysed, p.queued
    );
    println!(
        "  rated trades: {} finished, {} still open, {} skipped (no entry)",
        p.done, p.open, p.skipped
    );
    println!(
        "  spend: ${:.2} today, ${:.2} this month (caps ${:.2} / ${:.0}); next run ~{}",
        p.spent_today,
        p.spent_month,
        p.cap_day,
        p.cap_month,
        p.next_run_cost
            .map_or("unmeasured".into(), |c| format!("${c:.2}"))
    );
    if !p.missed.is_empty() {
        println!(
            "  MISSED sessions (no signals logged): {}",
            p.missed.join(", ")
        );
    }
    println!(
        "  verdict: {}",
        match p.verdict {
            "not due" => format!(
                "not due (at {} rated signals, or {} days after {})",
                desk::TARGET_SIGNALS,
                desk::MAX_DAYS,
                p.first.as_deref().unwrap_or("the first analysis")
            ),
            "waiting" => "sample fixed; waiting for its trades to finish".into(),
            _ => "READY: run `bmv-trader desk-verdict`".into(),
        }
    );
    Ok(p.cards)
}

/// The trials logged so far, by name; a log that can't be read is an error, not "none".
fn logged_trials(log: &Path) -> Result<Vec<String>, String> {
    let text = match std::fs::read_to_string(log) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", log.display())),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<stats::Trial>(l)
                .map(|t| t.strategy)
                .map_err(|e| format!("{}: {e}", log.display()))
        })
        .collect()
}

/// Trial 10's verdict on its fixed sample, once every trade in it has finished; logged once.
fn desk_verdict() -> Result<(), String> {
    use bmv_trader::desk;
    let (home, dir) = (config::home()?, data_dir()?);
    let _lock = desk::lock(&home)?;
    let today = et_now().date_naive();
    let log = dir.join("trials.jsonl");
    let trials = logged_trials(&log)?;
    if trials.iter().any(|t| t == desk::TRIAL_NAME) {
        return Err("trial 10 is already logged".into());
    }
    // the lab logs trial 11 before trial 10's verdict is due (DESIGN §7h); it doesn't count here
    let before = trials
        .iter()
        .filter(|t| t.as_str() != bmv_trader::lab::TRIAL_11)
        .count();
    if before + 1 != desk::TRIAL_NUMBER {
        return Err(format!(
            "trials.jsonl holds {before} trials before trial 10 (lab excluded); trial 10 must be the 10th (DESIGN §7f)"
        ));
    }
    let (_, to) = bmv_trader::scan::daily_range(&dir, bmv_trader::scan::ADJUSTED_SIP_LIVE)?;
    let fresh = history::canonical_date(to.get(..10).unwrap_or(&to))?;
    if (today - fresh).num_days() > 4 {
        return Err(format!("the live bars end {to}: run `desk-run` first"));
    }
    let cal = live_calendar(&dir)?;
    let cards = desk_report(&home, &cal)?;
    let analyses = desk::read_analyses(&home)?;
    let first = desk::first_analysis(&analyses);
    let sample = desk::sample(&cards, first.as_deref(), &today.to_string())
        .ok_or("not due yet: the verdict waits for its pre-registered sample")?;
    if sample
        .iter()
        .any(|c| c.scored == bmv_trader::swing::Scored::Pending)
    {
        return Err("the sample's trades haven't all finished yet".into());
    }
    let t = desk::tally(&sample);
    let gate = desk::verdict(&sample, desk::TRIAL_NUMBER);
    let alpha = stats::FAMILY_ALPHA / desk::TRIAL_NUMBER as f64;
    let trial = stats::Trial {
        when: chrono::Utc::now().to_rfc3339(),
        strategy: desk::TRIAL_NAME.into(),
        params: format!(
            "TradingAgents veto on trial 9 signals; {} / {}; sample {} rated ({} finished, {} skipped); units: expectancy_r = AI-weighted mean R, ci_low = lower bound of D",
            desk::QUICK_MODEL,
            desk::DEEP_MODEL,
            sample.len(),
            t.done,
            t.skipped
        ),
        period: format!("{}..{today}", first.unwrap_or_default()),
        trades: t.done,
        expectancy_r: t.weighted.unwrap_or(0.0),
        profit_factor: None,
        max_drawdown_r: 0.0,
        ci_low: desk::d_lower_bound(&sample, alpha, stats::resamples_for(alpha)),
    };
    let logged = stats::record_trial(&log, &trial)?;
    let r = |x: Option<f64>| x.map_or("—".into(), |x| format!("{x:+.3}R"));
    println!(
        "trial {logged}: {}; all {}, approved {}, vetoed {}, AI-weighted {}, D {}",
        desk::TRIAL_NAME,
        r(t.mean_all),
        r(t.mean_approved),
        r(t.mean_vetoed),
        r(t.weighted),
        r(t.d)
    );
    for c in &gate.checks {
        println!(
            "  [{}] {}: {}",
            if c.passed { "pass" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    println!(
        "verdict: {}",
        if gate.passed() {
            "the AI veto earns its next step"
        } else {
            "the AI adds nothing measurable"
        }
    );
    Ok(())
}

/// One analyst run outside the trial, to measure cost; it still counts toward the budget. Each
/// run gets a fresh memory folder, so it can't influence trial ratings.
fn desk_analyse(symbol: &str, date: &str) -> Result<(), String> {
    use bmv_trader::desk::{self, Analysis, Status};
    use bmv_trader::swing::Signal;
    let home = config::home()?;
    let _lock = desk::lock(&home)?;
    let today = et_now().date_naive().to_string();
    let key = desk::anthropic_key()?
        .ok_or("add ANTHROPIC_API_KEY to the Keychain first (service bmv-trader)")?;
    if let Some(r) = desk::over_budget(&desk::all_runs(&home)?, &today) {
        return Err(r);
    }
    let signal = Signal {
        symbol: symbol.into(),
        date: date.into(),
        ratio: 0.0,
        close: 0.0,
        atr: 0.0,
    };
    // the spend is recorded before the answer is shown, so a failed write can't hide it
    let (status, calls) = desk::analyse(&home, &desk::Analyst::Claude(&key), symbol, date);
    desk::record_manual(
        &home,
        &Analysis {
            when: et_now().to_rfc3339(),
            signal,
            rank: 0,
            status: status.clone(),
        },
    )?;
    for c in &calls {
        println!(
            "  {} in {} out {} ${:.4}",
            c.model,
            c.input,
            c.output,
            desk::call_cost(c)
        );
    }
    println!("{symbol} {date}: {}", describe(&status));
    if let Status::Rated { summary, .. } = &status {
        println!("{summary}");
    }
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
    let desk = bmv_trader::desk_link::Link::from_env()?;
    // a halt the desk should know about: the drawdown halt at startup, or a run that stopped
    let report = |why: &str| {
        if let Some(d) = &desk
            && let Err(e) = d.report_halt(why)
        {
            eprintln!("desk: couldn't report the halt: {e}");
        }
    };
    let mut r = live::startup(
        broker::Alpaca::new(cfg)?,
        mode,
        &home,
        settings,
        symbols.to_vec(),
        desk.as_ref(),
    )
    .inspect_err(|e| {
        if e.starts_with("drawdown halt") {
            report(e)
        }
    })?;
    live::run(&mut r, &home).inspect_err(|e| report(e))?;
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
