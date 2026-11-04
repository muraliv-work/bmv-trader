//! Trial 10: the AI-veto forward test (DESIGN §7f). Each night: trial 9's signals on the session's
//! close, a TradingAgents rating for the top ones within budget, and scores for older signals from
//! the bars that came after them. Shadow only: nothing here can place an order.

use crate::momentum::Calendar;
use crate::scan::{self, ADJUSTED_SIP_LIVE};
use crate::stats;
use crate::swing::{self, Scored, Signal};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The pre-registered limits (DESIGN §7f, DECISION A12).
pub const MAX_PER_DAY: usize = 6;
pub const MONTHLY_BUDGET_USD: f64 = 100.0;
pub const DAILY_BUDGET_USD: f64 = 3.30;
pub const TARGET_SIGNALS: usize = 150;
pub const MAX_DAYS: i64 = 183;
/// Trial 10 is judged at 1 − 0.05/10, whatever else is logged later.
pub const TRIAL_NUMBER: usize = 10;
pub const QUICK_MODEL: &str = "claude-haiku-5-5";
pub const DEEP_MODEL: &str = "claude-sonnet-5-5";
pub const TRIAL_NAME: &str = "ai-veto forward";
/// The pinned sidecar (sidecars/tradingagents/requirements.txt).
pub const TRADINGAGENTS_VERSION: &str = "0.7.1";
const TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Trial 12's local analyst (DESIGN §7i), both tiers, reasoning off; local runs get longer.
pub const LOCAL_MODEL: &str = "qwen3:8b";
const LOCAL_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const SUMMARY_CHARS: usize = 2000;
/// The most stdout read from the sidecar; its answer is one short line.
const MAX_OUTPUT: u64 = 1 << 20;
/// The sidecar marks its answer line with this, so stray library output can't be mistaken for it.
const RESULT_PREFIX: &str = "BMV_RESULT ";
/// The sidecar's exit code when Anthropic refused the request before any call was billed.
const REJECTED_EXIT: i32 = 4;

/// TradingAgents' five-tier rating.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rating {
    Buy,
    Overweight,
    Hold,
    Underweight,
    Sell,
}

impl Rating {
    /// Exactly the five tiers; "REVIEW" (no parseable rating) or anything else is None.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Buy" => Rating::Buy,
            "Overweight" => Rating::Overweight,
            "Hold" => Rating::Hold,
            "Underweight" => Rating::Underweight,
            "Sell" => Rating::Sell,
            _ => return None,
        })
    }

    /// How much of the trade the verdict keeps (DESIGN §7f).
    pub fn weight(self) -> f64 {
        match self {
            Rating::Buy | Rating::Overweight => 1.0,
            Rating::Hold => 0.5,
            Rating::Underweight | Rating::Sell => 0.0,
        }
    }
}

/// One LLM call as the sidecar reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub model: String,
    pub input: u64,
    pub output: u64,
}

/// The sidecar's answer. Unknown fields are refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarOut {
    ticker: String,
    date: String,
    rating: String,
    summary: String,
    usage: Vec<Call>,
    version: String,
}

/// Dollars for one call at Anthropic's list prices (claude-api reference, 2026-10). Input is
/// always charged at the full rate (cache reads are cheaper), so this errs high. Unknown models are
/// charged at the Sonnet rate and flagged by `check_models`.
pub fn call_cost(c: &Call) -> f64 {
    let (i, o) = if c.model.starts_with(QUICK_MODEL) {
        if c.input > 100_000 {
            (0.50, 2.50)
        } else {
            (0.10, 0.50)
        }
    } else {
        (2.00, 10.00)
    };
    (c.input as f64 * i + c.output as f64 * o) / 1e6
}

/// A run must show real usage, only from the two pinned models, including the rating model.
fn check_models(calls: &[Call], (quick, deep): (&str, &str)) -> Result<(), String> {
    if let Some(c) = calls
        .iter()
        .find(|c| !(c.model.starts_with(quick) || c.model.starts_with(deep)))
    {
        return Err(format!("unexpected model {:?}", trim(&c.model)));
    }
    if !calls.iter().any(|c| c.model.starts_with(deep)) {
        return Err("no usage reported for the rating model".into());
    }
    Ok(())
}

/// At most 64 characters of untrusted text, for messages.
fn trim(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// What happened to one signal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Status {
    /// Logged before any spending, so a night cut short still records every signal.
    Queued,
    Rated {
        rating: Rating,
        summary: String,
        cost_usd: f64,
    },
    /// The analyst ran (and may have cost money) but gave no usable rating.
    Failed { reason: String, cost_usd: f64 },
    /// Not sent to the analyst: over the daily cap, over budget, or no key.
    Unanalysed { reason: String },
}

/// One line of `data/desk/analyses.jsonl`. A signal has a Queued line, then one final line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    pub when: String,
    pub signal: Signal,
    pub rank: usize,
    pub status: Status,
}

impl Analysis {
    pub fn cost(&self) -> f64 {
        match &self.status {
            Status::Rated { cost_usd, .. } | Status::Failed { cost_usd, .. } => *cost_usd,
            Status::Queued | Status::Unanalysed { .. } => 0.0,
        }
    }

    fn key(&self) -> (String, String) {
        (self.signal.date.clone(), self.signal.symbol.clone())
    }
}

/// Where trial 10 keeps its files.
pub fn desk_dir(home: &Path) -> PathBuf {
    home.join("data").join("desk")
}

fn analyses_path(home: &Path) -> PathBuf {
    desk_dir(home).join("analyses.jsonl")
}

/// Manual runs (`desk-analyse`) spend budget but aren't part of the trial.
fn manual_path(home: &Path) -> PathBuf {
    desk_dir(home).join("manual.jsonl")
}

/// Signals found for one session, written before any of them is analysed.
pub fn signals_path(home: &Path, date: &str) -> PathBuf {
    desk_dir(home).join("signals").join(format!("{date}.json"))
}

/// Every line, in file order. A final line without its newline was cut off mid-write (the
/// process died); it is dropped with a warning, since that run never finished.
pub(crate) fn read_lines(path: &Path) -> Result<Vec<Analysis>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let complete = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(l) {
            Ok(a) => out.push(a),
            Err(_) if !complete && i + 1 == lines.len() => {
                eprintln!("{}: ignoring a cut-off last line", path.display());
            }
            Err(e) => return Err(format!("{} line {}: {e}", path.display(), i + 1)),
        }
    }
    Ok(out)
}

/// Each signal's current state: its first final line, else its Queued line, in first-seen order.
/// Only the first answer counts, so a later line can never replace a rating.
pub fn latest(all: &[Analysis]) -> Vec<Analysis> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut by: BTreeMap<(String, String), Analysis> = BTreeMap::new();
    for a in all {
        let k = a.key();
        match by.get(&k) {
            None => {
                order.push(k.clone());
                by.insert(k, a.clone());
            }
            Some(prev) if prev.status == Status::Queued && a.status != Status::Queued => {
                by.insert(k, a.clone());
            }
            Some(_) => {}
        }
    }
    order.into_iter().filter_map(|k| by.remove(&k)).collect()
}

/// The trial's signals, one per (session, symbol).
pub fn read_analyses(home: &Path) -> Result<Vec<Analysis>, String> {
    Ok(latest(&read_lines(&analyses_path(home))?))
}

pub(crate) fn append(path: &Path, a: &Analysis) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    // a line cut off by a crash must not swallow the next one
    let torn = std::fs::read(path)
        .map(|b| b.last().is_some_and(|c| *c != b'\n'))
        .unwrap_or(false);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut line = if torn {
        "\n".to_string()
    } else {
        String::new()
    };
    line += &(serde_json::to_string(a).map_err(|e| e.to_string())? + "\n");
    f.write_all(line.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Records one trial line.
pub fn record(home: &Path, a: &Analysis) -> Result<(), String> {
    append(&analyses_path(home), a)
}

/// Records a manual run: it spends budget but isn't a trial signal.
pub fn record_manual(home: &Path, a: &Analysis) -> Result<(), String> {
    append(&manual_path(home), a)
}

/// Every trial and manual line, for the budget.
pub fn all_runs(home: &Path) -> Result<Vec<Analysis>, String> {
    let mut a = read_lines(&analyses_path(home))?;
    a.extend(read_lines(&manual_path(home))?);
    Ok(a)
}

/// Only one trial 10 command at a time, so two can't both spend today's budget or analyse one
/// signal twice. Held until dropped.
pub fn lock(home: &Path) -> Result<std::fs::File, String> {
    let dir = desk_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("run.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("desk lock {}: {e}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "another trial 10 command is running ({} is locked)",
            path.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("desk lock {}: {e}", path.display())),
    }
}

/// Money spent on ET date `today` and in its month, over trial and manual runs (`when` is ET).
pub fn spent(all: &[Analysis], today: &str) -> (f64, f64) {
    let on = |a: &&Analysis| a.when.get(..10) == Some(today);
    let month = |a: &&Analysis| a.when.get(..7) == today.get(..7);
    (
        all.iter().filter(on).map(Analysis::cost).sum(),
        all.iter().filter(month).map(Analysis::cost).sum(),
    )
}

/// The expected cost of the next run: the mean of past runs that cost something, or None before
/// the first one (which is then allowed, to measure it).
pub fn expected_cost(all: &[Analysis]) -> Option<f64> {
    let costs: Vec<f64> = all
        .iter()
        .map(Analysis::cost)
        .filter(|c| *c > 0.0)
        .collect();
    (!costs.is_empty()).then(|| costs.iter().sum::<f64>() / costs.len() as f64)
}

/// None if another run fits the budget, else why not.
pub fn over_budget(all: &[Analysis], today: &str) -> Option<String> {
    let (day, month) = spent(all, today);
    let next = expected_cost(all).unwrap_or(0.0);
    if day + next > DAILY_BUDGET_USD {
        Some(format!(
            "budget: ${day:.2} spent today, next ~${next:.2}, cap ${DAILY_BUDGET_USD:.2}"
        ))
    } else if month + next > MONTHLY_BUDGET_USD {
        Some(format!(
            "budget: ${month:.2} spent this month, cap ${MONTHLY_BUDGET_USD:.0}"
        ))
    } else {
        None
    }
}

/// Fails if `dir` or any folder above it holds a `.env` that `import tradingagents` would load.
fn no_dotenv_above(dir: &Path) -> Result<(), String> {
    for d in dir.ancestors() {
        for name in [".env", ".env.enterprise"] {
            if d.join(name).exists() {
                return Err(format!(
                    "{} exists; TradingAgents would load it, so the analyst won't run there",
                    d.join(name).display()
                ));
            }
        }
    }
    Ok(())
}

/// The environment the sidecar gets, nothing else from ours. `run` is a fresh folder per run: its
/// HOME, memory log and cache, so no run can read what an earlier one learned.
fn child_env(key: &str, workspace: Option<&str>, run: &Path) -> Vec<(&'static str, String)> {
    let at = |p: &str| run.join(p).display().to_string();
    let mut env = vec![
        ("ANTHROPIC_API_KEY", key.to_string()),
        ("HOME", run.display().to_string()),
        ("PATH", "/usr/bin:/bin".to_string()),
        ("TRADINGAGENTS_RESULTS_DIR", at("results")),
        ("TRADINGAGENTS_MEMORY_LOG_PATH", at("memory.md")),
        ("TRADINGAGENTS_CACHE_DIR", at("cache")),
        ("BMV_USAGE_FILE", at("usage.jsonl")),
        ("TMPDIR", at("tmp")),
        ("TMP", at("tmp")),
        ("TEMP", at("tmp")),
    ];
    if let Some(w) = workspace {
        env.push(("BMV_ANTHROPIC_WORKSPACE_ID", w.to_string()));
    }
    env
}

/// The local analyst's environment: trial 10's without any Anthropic key, plus the mode switch.
fn local_env(run: &Path) -> Vec<(&'static str, String)> {
    let mut env: Vec<_> = child_env("", None, run)
        .into_iter()
        .filter(|(k, _)| *k != "ANTHROPIC_API_KEY")
        .collect();
    env.push(("BMV_ANALYST", "local".to_string()));
    env
}

/// macOS sandbox for the sidecar (DECISION S23): it reads nothing in the user's home except its
/// run folder, the sidecar and uv's Python; never the project, the Keychain or `security`; and
/// writes only to its run folder (its HOME and TMPDIR). The network stays open: the analyst needs
/// Anthropic and its data, so what it can read is what it could leak.
fn sandbox_profile(project: &Path, sidecar: &Path, user_home: &Path, run: &Path) -> String {
    let q = |p: &Path| {
        p.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    };
    format!(
        "(version 1)\n(allow default)\n\
         (deny file-read-data (subpath \"{home}\"))\n\
         (deny file-read-data (subpath \"{project}\"))\n\
         (allow file-read-data (subpath \"{sidecar}\"))\n\
         (allow file-read-data (subpath \"{run}\"))\n\
         (allow file-read-data (subpath \"{python}\"))\n\
         (deny file-read* (subpath \"{keychains}\"))\n\
         (deny process-exec (literal \"/usr/bin/security\"))\n\
         (deny mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd\"))\n\
         (deny file-write*)\n\
         (allow file-write* (subpath \"{run}\") (subpath \"/dev\"))\n",
        home = q(user_home),
        project = q(project),
        sidecar = q(sidecar),
        run = q(run),
        // uv keeps the interpreter the sidecar's venv points at here
        python = q(&user_home
            .join(".local")
            .join("share")
            .join("uv")
            .join("python")),
        keychains = q(&user_home.join("Library").join("Keychains")),
    )
}

/// The sidecar's verdict on `ticker` at `date`, or why there isn't one, with what it cost.
pub fn analyse(home: &Path, analyst: &Analyst, ticker: &str, date: &str) -> (Status, Vec<Call>) {
    let (result, calls) = run_sidecar(home, analyst, ticker, date);
    let reported = analyst.cost(&calls);
    match result {
        Ok(out) => {
            // the answer's own usage is the full list; the file may lag it by nothing
            let calls = if out.usage.is_empty() {
                calls
            } else {
                out.usage.clone()
            };
            let cost = analyst.cost(&calls);
            let status = match validate(&out, ticker, date, &calls, analyst.models()) {
                Ok(rating) => Status::Rated {
                    rating,
                    summary: out.summary,
                    cost_usd: cost,
                },
                Err(reason) => Status::Failed {
                    reason,
                    cost_usd: cost.max(reported),
                },
            };
            (status, calls)
        }
        // refused before any billed call: nothing spent
        Err(Failure::Rejected(reason)) if reported == 0.0 => (
            Status::Failed {
                reason,
                cost_usd: 0.0,
            },
            calls,
        ),
        // a crashed run is charged what it reported, or a full day's budget if it reported nothing
        Err(Failure::Rejected(reason) | Failure::Other(reason)) => (
            Status::Failed {
                reason,
                cost_usd: if reported > 0.0 || matches!(analyst, Analyst::Local) {
                    reported
                } else {
                    DAILY_BUDGET_USD
                },
            },
            calls,
        ),
    }
}

fn validate(
    out: &SidecarOut,
    ticker: &str,
    date: &str,
    calls: &[Call],
    models: (&str, &str),
) -> Result<Rating, String> {
    if out.ticker != ticker || out.date != date {
        return Err(format!(
            "answer is for {} {}, asked {ticker} {date}",
            trim(&out.ticker),
            trim(&out.date)
        ));
    }
    if out.summary.chars().count() > SUMMARY_CHARS {
        return Err("summary longer than 2000 characters".into());
    }
    if out.version != TRADINGAGENTS_VERSION {
        return Err(format!(
            "analyst is TradingAgents {}, pinned {TRADINGAGENTS_VERSION}",
            trim(&out.version)
        ));
    }
    check_models(calls, models)?;
    Rating::parse(&out.rating).ok_or_else(|| format!("no usable rating: {:?}", trim(&out.rating)))
}

/// Usage the sidecar wrote call by call, so a crashed run can still be priced. The sidecar
/// controls this file: only a regular file is read, and only its first 1 MB.
fn read_usage(path: &Path) -> Vec<Call> {
    let regular = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    let mut text = String::new();
    if let Some(f) = regular.then(|| std::fs::File::open(path).ok()).flatten() {
        let _ = f.take(MAX_OUTPUT).read_to_string(&mut text);
    }
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Kills the sidecar's whole process group, so a helper it started can't keep spending.
fn kill_group(child: &mut std::process::Child) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Why the sidecar gave no answer.
enum Failure {
    /// Anthropic refused the request (bad key, missing workspace) before any billed call.
    Rejected(String),
    Other(String),
}

impl From<String> for Failure {
    fn from(s: String) -> Self {
        Failure::Other(s)
    }
}

impl From<&str> for Failure {
    fn from(s: &str) -> Self {
        Failure::Other(s.to_string())
    }
}

/// The Anthropic key, and the workspace ID a key not scoped to a workspace needs.
pub struct Credentials {
    pub key: String,
    pub workspace: Option<String>,
}

/// Who rates a signal: Claude for trial 10, or the local model for trial 12 (DECISION S26).
pub enum Analyst<'a> {
    Claude(&'a Credentials),
    Local,
}

impl Analyst<'_> {
    /// (quick tier, deep tier that gives the rating)
    fn models(&self) -> (&'static str, &'static str) {
        match self {
            Analyst::Claude(_) => (QUICK_MODEL, DEEP_MODEL),
            Analyst::Local => (LOCAL_MODEL, LOCAL_MODEL),
        }
    }

    fn cost(&self, calls: &[Call]) -> f64 {
        match self {
            Analyst::Claude(_) => calls.iter().map(call_cost).sum(),
            Analyst::Local => 0.0,
        }
    }

    fn timeout(&self) -> Duration {
        match self {
            Analyst::Claude(_) => TIMEOUT,
            Analyst::Local => LOCAL_TIMEOUT,
        }
    }

    fn env(&self, run: &Path) -> Vec<(&'static str, String)> {
        match self {
            Analyst::Claude(k) => child_env(&k.key, k.workspace.as_deref(), run),
            Analyst::Local => local_env(run),
        }
    }
}

fn run_sidecar(
    home: &Path,
    analyst: &Analyst,
    ticker: &str,
    date: &str,
) -> (Result<SidecarOut, Failure>, Vec<Call>) {
    let user_home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return (Err("HOME is not set".into()), Vec::new()),
    };
    let run = user_home
        .join("Library")
        .join("Caches")
        .join("bmv-trader")
        .join("ta")
        .join(format!("{date}-{ticker}-{}", std::process::id()));
    let usage_file = run.join("usage.jsonl");
    let result = spawn_and_wait(home, analyst, ticker, date, &user_home, &run);
    let calls = read_usage(&usage_file);
    let _ = std::fs::remove_dir_all(&run);
    (result, calls)
}

fn spawn_and_wait(
    home: &Path,
    analyst: &Analyst,
    ticker: &str,
    date: &str,
    user_home: &Path,
    run: &Path,
) -> Result<SidecarOut, Failure> {
    use std::os::unix::process::CommandExt;
    if !crate::history::valid_symbol(ticker) {
        return Err(format!("invalid ticker {:?}", trim(ticker)).into());
    }
    crate::history::canonical_date(date).map_err(Failure::Other)?;
    let project = std::fs::canonicalize(home).map_err(|e| format!("project path: {e}"))?;
    let sidecar = project.join("sidecars").join("tradingagents");
    let python = sidecar.join(".venv").join("bin").join("python");
    std::fs::create_dir_all(run).map_err(|e| format!("cannot create {}: {e}", run.display()))?;
    // outside the project and with no .env above it, so TradingAgents' load_dotenv() finds none
    let run = std::fs::canonicalize(run).map_err(|e| format!("run folder: {e}"))?;
    std::fs::create_dir_all(run.join("tmp")).map_err(|e| format!("run tmp folder: {e}"))?;
    if run.starts_with(&project) {
        return Err("the analyst's folder is inside the project".into());
    }
    no_dotenv_above(&run)?;
    let logs = desk_dir(home).join("ta-logs");
    std::fs::create_dir_all(&logs).map_err(|e| format!("cannot create {}: {e}", logs.display()))?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join(format!("{date}-{ticker}.log")))
        .map_err(|e| format!("cannot open the analyst log: {e}"))?;
    let profile = sandbox_profile(&project, &sidecar, user_home, &run);
    let mut cmd = Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg(&python)
        .arg("-I")
        .arg(sidecar.join("run.py"))
        .arg(ticker)
        .arg(date)
        .current_dir(&run)
        .env_clear()
        .envs(analyst.env(&run))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start the analyst: {e}"))?;
    let stdout = child.stdout.take().ok_or("no stdout from the analyst")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.take(MAX_OUTPUT).read_to_string(&mut s).map(|_| s)
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > analyst.timeout() => {
                kill_group(&mut child);
                return Err(format!(
                    "timed out after {} minutes",
                    analyst.timeout().as_secs() / 60
                )
                .into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(e) => {
                kill_group(&mut child);
                return Err(format!("waiting for the analyst: {e}").into());
            }
        }
    };
    // a helper still holding stdout open would block the reader: end the group first
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stderr(Stdio::null())
        .status();
    let text = reader
        .join()
        .map_err(|_| "analyst output reader panicked".to_string())?
        .map_err(|e| format!("cannot read the analyst output: {e}"))?;
    if status.code() == Some(REJECTED_EXIT) {
        return Err(Failure::Rejected(
            "Anthropic refused the request: check the key and workspace (see data/desk/ta-logs)"
                .into(),
        ));
    }
    if !status.success() {
        return Err(format!("analyst exited with {status} (see data/desk/ta-logs)").into());
    }
    let line = text
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(RESULT_PREFIX))
        .ok_or("analyst gave no answer line")?;
    serde_json::from_str(line).map_err(|e| format!("analyst output: {e}").into())
}

/// The Anthropic key, from the Keychain only (service `bmv-trader`, account `ANTHROPIC_API_KEY`),
/// with the optional `ANTHROPIC_WORKSPACE_ID` beside it.
pub fn anthropic_key() -> Result<Option<Credentials>, String> {
    let Some(key) = crate::config::keychain("ANTHROPIC_API_KEY")? else {
        return Ok(None);
    };
    Ok(Some(Credentials {
        key,
        workspace: crate::config::keychain("ANTHROPIC_WORKSPACE_ID")?,
    }))
}

/// The session's signals from the live bars, best volume ratio first, then symbol. SPY is only the
/// calendar; trial 9 leaves funds out.
pub fn find_signals(home: &Path, cal: &Calendar) -> Result<Vec<Signal>, String> {
    let dir = home.join("data");
    let mut out = Vec::new();
    scan::for_each_daily(&dir, ADJUSTED_SIP_LIVE, &mut |s, bars| {
        if s == crate::momentum::BENCHMARK {
            return Ok(());
        }
        if let Some(sig) = swing::signal_today(s, &bars, cal, &swing::TRIAL_9) {
            out.push(sig);
        }
        Ok(())
    })?;
    out.sort_by(|a, b| {
        b.ratio
            .total_cmp(&a.ratio)
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    Ok(out)
}

/// A rated signal and how it came out.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Scorecard {
    pub date: String,
    pub rank: usize,
    pub symbol: String,
    pub rating: Rating,
    pub scored: Scored,
}

/// Scores every rated signal against the live bars; a symbol whose bars can't be read is an error.
pub fn score_all(
    home: &Path,
    cal: &Calendar,
    analyses: &[Analysis],
) -> Result<Vec<Scorecard>, String> {
    let dir = home.join("data");
    let mut out = Vec::new();
    for a in analyses {
        let Status::Rated { rating, .. } = a.status else {
            continue;
        };
        let bars = scan::read_daily(&dir, ADJUSTED_SIP_LIVE, &a.signal.symbol)?;
        out.push(Scorecard {
            date: a.signal.date.clone(),
            rank: a.rank,
            symbol: a.signal.symbol.clone(),
            rating,
            scored: swing::score_forward(&bars, cal, &a.signal),
        });
    }
    Ok(out)
}

/// The date of the first analysis (rated or failed), from which the 6 months run.
pub fn first_analysis(analyses: &[Analysis]) -> Option<String> {
    analyses
        .iter()
        .filter(|a| matches!(a.status, Status::Rated { .. } | Status::Failed { .. }))
        .map(|a| a.signal.date.clone())
        .min()
}

fn days_between(a: &str, b: &str) -> Option<i64> {
    let a = crate::history::canonical_date(a).ok()?;
    let b = crate::history::canonical_date(b).ok()?;
    Some((b - a).num_days())
}

/// The fixed verdict sample, by signal date: the first 150 rated signals, or once 6 months have
/// passed, every rated signal from those 6 months. None until either holds. Defined by date and
/// rank only, never by outcome, so fast exits can't fill it first.
pub fn sample(cards: &[Scorecard], first: Option<&str>, today: &str) -> Option<Vec<Scorecard>> {
    let mut c = cards.to_vec();
    c.sort_by(|a, b| a.date.cmp(&b.date).then(a.rank.cmp(&b.rank)));
    if c.len() >= TARGET_SIGNALS {
        c.truncate(TARGET_SIGNALS);
        return Some(c);
    }
    let first = first?;
    (days_between(first, today)? >= MAX_DAYS).then(|| {
        c.into_iter()
            .filter(|x| days_between(first, &x.date).is_some_and(|d| d < MAX_DAYS))
            .collect()
    })
}

/// Where trial 10 stands, for `desk-report` and the console. Counts and ratings only: outcomes by
/// rating stay hidden until the verdict, so nobody reads the result early (DESIGN §7f).
#[derive(Debug, Serialize)]
pub struct Progress {
    pub signals: usize,
    pub rated: usize,
    pub failed: usize,
    pub unanalysed: usize,
    pub queued: usize,
    /// Rated trades: finished, still open, skipped (no entry).
    pub done: usize,
    pub open: usize,
    pub skipped: usize,
    pub spent_today: f64,
    pub spent_month: f64,
    pub cap_day: f64,
    pub cap_month: f64,
    pub next_run_cost: Option<f64>,
    pub target: usize,
    /// The first analysed signal's date, from which the 6 months run.
    pub first: Option<String>,
    /// The last day the 6-month window can run to, once the first analysis exists.
    pub due_by: Option<String>,
    /// "not due", "waiting" (sample fixed, trades still open) or "ready".
    pub verdict: &'static str,
    /// Sessions since the first signal with no signals logged (the Mac was off).
    pub missed: Vec<String>,
    /// The latest night: (symbol, rating or state), by rank.
    pub night: Option<(String, Vec<(String, String)>)>,
    #[serde(skip)]
    pub cards: Vec<Scorecard>,
}

pub fn progress(home: &Path, cal: &Calendar, today: &str) -> Result<Progress, String> {
    let analyses = read_analyses(home)?;
    let cards = score_all(home, cal, &analyses)?;
    let t = tally(&cards);
    let count = |f: &dyn Fn(&Status) -> bool| analyses.iter().filter(|a| f(&a.status)).count();
    let runs = all_runs(home)?;
    let (day, month) = spent(&runs, today);
    let first = first_analysis(&analyses);
    let start = analyses.iter().map(|a| a.signal.date.clone()).min();
    let missed = cal
        .sessions
        .iter()
        .filter(|d| start.as_ref().is_some_and(|s| *d >= s))
        .filter(|d| !signals_path(home, d).exists())
        .cloned()
        .collect();
    let verdict = match sample(&cards, first.as_deref(), today) {
        None => "not due",
        Some(s) if s.iter().any(|c| c.scored == Scored::Pending) => "waiting",
        Some(_) => "ready",
    };
    let due_by = first
        .as_deref()
        .and_then(|f| crate::history::canonical_date(f).ok())
        .map(|f| (f + chrono::Duration::days(MAX_DAYS)).to_string());
    let night = analyses
        .iter()
        .map(|a| a.signal.date.clone())
        .max()
        .map(|d| {
            let mut rows: Vec<&Analysis> = analyses.iter().filter(|a| a.signal.date == d).collect();
            rows.sort_by_key(|a| a.rank);
            let label = |s: &Status| match s {
                Status::Rated { rating, .. } => format!("{rating:?}"),
                Status::Failed { .. } => "failed".into(),
                Status::Unanalysed { .. } => "not analysed".into(),
                Status::Queued => "queued".into(),
            };
            let rows = rows
                .iter()
                .map(|a| (a.signal.symbol.clone(), label(&a.status)))
                .collect();
            (d, rows)
        });
    Ok(Progress {
        signals: analyses.len(),
        rated: count(&|s| matches!(s, Status::Rated { .. })),
        failed: count(&|s| matches!(s, Status::Failed { .. })),
        unanalysed: count(&|s| matches!(s, Status::Unanalysed { .. })),
        queued: count(&|s| *s == Status::Queued),
        done: t.done,
        open: t.pending,
        skipped: t.skipped,
        spent_today: day,
        spent_month: month,
        cap_day: DAILY_BUDGET_USD,
        cap_month: MONTHLY_BUDGET_USD,
        next_run_cost: expected_cost(&runs),
        target: TARGET_SIGNALS,
        first,
        due_by,
        verdict,
        missed,
        night,
        cards,
    })
}

/// Means over finished trades: all, approved (weight 1), vetoed (weight 0), AI-weighted, and D.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tally {
    pub done: usize,
    pub pending: usize,
    pub skipped: usize,
    pub mean_all: Option<f64>,
    pub mean_approved: Option<f64>,
    pub mean_vetoed: Option<f64>,
    pub weighted: Option<f64>,
    pub d: Option<f64>,
}

fn done(cards: &[Scorecard]) -> Vec<(String, f64, f64)> {
    cards
        .iter()
        .filter_map(|c| match c.scored {
            Scored::Done { r, .. } => Some((c.date.clone(), c.rating.weight(), r)),
            _ => None,
        })
        .collect()
}

/// D = Σ w·r / Σ w − mean r, None if nothing is kept or nothing is done.
fn d_of(rows: &[(f64, f64)]) -> Option<f64> {
    let w: f64 = rows.iter().map(|(w, _)| w).sum();
    (w > 0.0 && !rows.is_empty()).then(|| {
        rows.iter().map(|(w, r)| w * r).sum::<f64>() / w
            - rows.iter().map(|(_, r)| r).sum::<f64>() / rows.len() as f64
    })
}

pub fn tally(cards: &[Scorecard]) -> Tally {
    let rows = done(cards);
    let mean = |xs: Vec<f64>| (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64);
    let wr: Vec<(f64, f64)> = rows.iter().map(|(_, w, r)| (*w, *r)).collect();
    let w: f64 = wr.iter().map(|(w, _)| w).sum();
    Tally {
        done: rows.len(),
        pending: cards.iter().filter(|c| c.scored == Scored::Pending).count(),
        skipped: cards
            .iter()
            .filter(|c| matches!(c.scored, Scored::Skipped(_)))
            .count(),
        mean_all: mean(rows.iter().map(|x| x.2).collect()),
        mean_approved: mean(rows.iter().filter(|x| x.1 == 1.0).map(|x| x.2).collect()),
        mean_vetoed: mean(rows.iter().filter(|x| x.1 == 0.0).map(|x| x.2).collect()),
        weighted: (w > 0.0).then(|| wr.iter().map(|(w, r)| w * r).sum::<f64>() / w),
        d: d_of(&wr),
    }
}

/// One-sided lower bound of D at level 1 − alpha, resampling whole signal days. Resamples with
/// nothing kept have no D and are counted as failures, so they can't lift the bound.
pub fn d_lower_bound(cards: &[Scorecard], alpha: f64, iters: usize) -> Option<f64> {
    let mut by_day: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for (d, w, r) in done(cards) {
        by_day.entry(d).or_default().push((w, r));
    }
    let days: Vec<Vec<(f64, f64)>> = by_day.into_values().collect();
    if days.is_empty() {
        return None;
    }
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut ds: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let mut rows = Vec::new();
        for _ in 0..days.len() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rows.extend_from_slice(&days[(rng % days.len() as u64) as usize]);
        }
        ds.push(d_of(&rows).unwrap_or(f64::NEG_INFINITY));
    }
    ds.sort_by(f64::total_cmp);
    Some(ds[((alpha * (ds.len() - 1) as f64).floor() as usize).min(ds.len() - 1)])
}

/// The trial 10 gate (DESIGN §7f), on the fixed sample once every trade in it has finished.
/// The gate for trial 10, or for trial 12, which uses trial 10's rules at its own level.
pub fn verdict(sample: &[Scorecard], trial: usize) -> stats::GateReport {
    let t = tally(sample);
    let alpha = stats::FAMILY_ALPHA / trial as f64;
    let lo = d_lower_bound(sample, alpha, stats::resamples_for(alpha));
    let check = |name: &'static str, passed: bool, detail: String| stats::GateCheck {
        name,
        passed,
        detail,
    };
    stats::GateReport {
        checks: vec![
            check(
                "lower bound of D (AI-weighted minus plain mean R) > 0",
                lo.is_some_and(|x| x > 0.0),
                format!(
                    "{} at {:.3}% one-sided, trial {trial}",
                    lo.map_or("no bound".into(), |x| format!("{x:+.3}R")),
                    100.0 * (1.0 - alpha)
                ),
            ),
            check(
                "vetoed signals average below approved ones",
                matches!((t.mean_vetoed, t.mean_approved), (Some(v), Some(a)) if v < a),
                format!("vetoed {:?}, approved {:?}", t.mean_vetoed, t.mean_approved),
            ),
            check(
                "AI-weighted expectancy > 0",
                t.weighted.is_some_and(|w| w > 0.0),
                format!("{:?}", t.weighted),
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratings_parse_exactly() {
        assert_eq!(Rating::parse("Buy"), Some(Rating::Buy));
        assert_eq!(Rating::parse("Sell").map(Rating::weight), Some(0.0));
        assert_eq!(Rating::parse("Hold").map(Rating::weight), Some(0.5));
        for bad in ["REVIEW", "Strong Buy", "buy", " Buy", ""] {
            assert_eq!(Rating::parse(bad), None, "{bad:?}");
        }
    }

    fn out(json: &str) -> Result<SidecarOut, serde_json::Error> {
        serde_json::from_str(json)
    }

    const OK: &str = r#"{"ticker":"AAPL","date":"2026-11-03","rating":"Buy","summary":"s","usage":[{"model":"claude-haiku-5-5","input":10,"output":5},{"model":"claude-sonnet-5-5","input":10,"output":5}],"version":"0.7.1"}"#;

    #[test]
    fn sidecar_output_is_strict() {
        let o = out(OK).unwrap();
        assert_eq!(
            validate(
                &o,
                "AAPL",
                "2026-11-03",
                &o.usage,
                (QUICK_MODEL, DEEP_MODEL)
            ),
            Ok(Rating::Buy)
        );
        assert!(
            validate(
                &o,
                "MSFT",
                "2026-11-03",
                &o.usage,
                (QUICK_MODEL, DEEP_MODEL)
            )
            .is_err(),
            "wrong ticker"
        );
        let extra = OK.replace(r#""version""#, r#""size":10,"version""#);
        assert!(out(&extra).is_err(), "unknown field refused");
        for (from, to, why) in [
            (r#""Buy""#, r#""Strong Buy""#, "invented rating"),
            ("claude-haiku-5-5", "gpt-6-luna", "non-Claude model"),
            (r#""0.7.1""#, r#""0.8.0""#, "unpinned version"),
        ] {
            let o = out(&OK.replace(from, to)).unwrap();
            assert!(
                validate(
                    &o,
                    "AAPL",
                    "2026-11-03",
                    &o.usage,
                    (QUICK_MODEL, DEEP_MODEL)
                )
                .is_err(),
                "{why}"
            );
        }
        let o = out(OK).unwrap();
        assert!(
            validate(&o, "AAPL", "2026-11-03", &[], (QUICK_MODEL, DEEP_MODEL)).is_err(),
            "no usage"
        );
        assert!(
            validate(
                &o,
                "AAPL",
                "2026-11-03",
                &o.usage[..1],
                (QUICK_MODEL, DEEP_MODEL)
            )
            .is_err(),
            "no rating-model call"
        );
        let long = OK.replace(
            r#""summary":"s""#,
            &format!(r#""summary":"{}""#, "x".repeat(2001)),
        );
        let o = out(&long).unwrap();
        assert!(
            validate(
                &o,
                "AAPL",
                "2026-11-03",
                &o.usage,
                (QUICK_MODEL, DEEP_MODEL)
            )
            .is_err()
        );
    }

    fn run(when: &str, cost: f64) -> Analysis {
        Analysis {
            when: when.into(),
            signal: Signal {
                symbol: "X".into(),
                date: when[..10].into(),
                ratio: 2.0,
                close: 10.0,
                atr: 0.5,
            },
            rank: 1,
            status: Status::Failed {
                reason: "r".into(),
                cost_usd: cost,
            },
        }
    }

    #[test]
    fn costs_and_budget() {
        let haiku = Call {
            model: QUICK_MODEL.into(),
            input: 100_000,
            output: 1_000_000,
        };
        assert!(
            (call_cost(&haiku) - 0.51).abs() < 1e-12,
            "0.01 in + 0.50 out"
        );
        let big = Call {
            input: 200_000,
            output: 0,
            ..haiku.clone()
        };
        assert!(
            (call_cost(&big) - 0.10).abs() < 1e-12,
            "long prompts at the higher rate"
        );
        let sonnet = Call {
            model: DEEP_MODEL.into(),
            input: 1_000_000,
            output: 100_000,
        };
        assert!((call_cost(&sonnet) - 3.0).abs() < 1e-12);

        assert_eq!(
            over_budget(&[], "2026-11-03"),
            None,
            "the first run is allowed"
        );
        let today = vec![
            run("2026-11-03T17:30:00-05:00", 1.5),
            run("2026-11-03T17:40:00-05:00", 1.5),
        ];
        assert!(
            over_budget(&today, "2026-11-03").is_some(),
            "3.0 + 1.5 > 3.30"
        );
        assert_eq!(over_budget(&today, "2026-11-04"), None);
        let month: Vec<Analysis> = (1..=30)
            .map(|d| run(&format!("2026-11-{d:02}T17:30:00-05:00"), 3.3))
            .collect();
        assert!(over_budget(&month, "2026-11-30").is_some());
    }

    #[test]
    fn the_local_analyst_gets_no_key_costs_nothing_and_checks_its_own_model() {
        let env = local_env(Path::new("/tmp/run"));
        assert!(
            env.iter()
                .all(|(k, _)| !k.starts_with("ANTHROPIC") && !k.starts_with("ALPACA"))
        );
        assert!(env.contains(&("BMV_ANALYST", "local".to_string())));
        let local = Analyst::Local;
        let used = [Call {
            model: LOCAL_MODEL.into(),
            input: 5000,
            output: 9000,
        }];
        assert_eq!(local.cost(&used), 0.0);
        assert!(check_models(&used, local.models()).is_ok());
        assert!(
            check_models(&used, (QUICK_MODEL, DEEP_MODEL)).is_err(),
            "trial 10 refuses it"
        );
        let claude = [Call {
            model: DEEP_MODEL.into(),
            input: 1,
            output: 1,
        }];
        assert!(
            check_models(&claude, local.models()).is_err(),
            "trial 12 refuses Claude"
        );
        assert!(local.timeout() > TIMEOUT);
        let k = Credentials {
            key: "k".into(),
            workspace: None,
        };
        assert_eq!(
            Analyst::Claude(&k).env(Path::new("/tmp/run")),
            child_env("k", None, Path::new("/tmp/run"))
        );
    }

    #[test]
    fn child_env_holds_only_what_it_needs() {
        let env = child_env("k", None, Path::new("/tmp/run"));
        let names: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            vec![
                "ANTHROPIC_API_KEY",
                "HOME",
                "PATH",
                "TRADINGAGENTS_RESULTS_DIR",
                "TRADINGAGENTS_MEMORY_LOG_PATH",
                "TRADINGAGENTS_CACHE_DIR",
                "BMV_USAGE_FILE",
                "TMPDIR",
                "TMP",
                "TEMP"
            ]
        );
        assert!(env.iter().all(|(k, v)| !k.starts_with("ALPACA")
            && *k != "ANTHROPIC_BASE_URL"
            && (*k == "ANTHROPIC_API_KEY" || *k == "PATH" || v.starts_with("/tmp/run"))));
        let ws = child_env("k", Some("wrkspc_1"), Path::new("/tmp/run"));
        assert_eq!(
            ws.last(),
            Some(&("BMV_ANTHROPIC_WORKSPACE_ID", "wrkspc_1".to_string()))
        );
        let p = sandbox_profile(
            Path::new("/p"),
            Path::new("/p/s"),
            Path::new("/u"),
            Path::new("/r"),
        );
        assert!(
            p.contains("(deny file-write*)\n"),
            "writes denied by default"
        );
        assert!(
            p.contains("(allow file-write* (subpath \"/r\")"),
            "only the run folder"
        );
        assert!(p.contains("(deny file-read-data (subpath \"/p\"))"));
        assert!(p.contains("(allow file-read-data (subpath \"/p/s\"))"));
        assert!(p.contains("/u/Library/Keychains"));
    }

    #[test]
    fn dotenv_above_the_run_folder_blocks_the_run() {
        let base = std::env::temp_dir().join(format!("bmv-desk-{}", std::process::id()));
        let deep = base.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(base.join(".env"), "X=1").unwrap();
        assert!(no_dotenv_above(&deep).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn latest_keeps_the_first_answer_and_survives_a_torn_line() {
        let q = |sym: &str| Analysis {
            status: Status::Queued,
            signal: Signal {
                symbol: sym.into(),
                ..run("2026-11-03T17:30:00-05:00", 0.0).signal
            },
            ..run("2026-11-03T17:30:00-05:00", 0.0)
        };
        let rated = |sym: &str, r: Rating| Analysis {
            status: Status::Rated {
                rating: r,
                summary: String::new(),
                cost_usd: 0.5,
            },
            ..q(sym)
        };
        let all = vec![
            q("A"),
            q("B"),
            rated("A", Rating::Buy),
            rated("A", Rating::Sell),
        ];
        let l = latest(&all);
        assert_eq!(l.len(), 2);
        assert_eq!(
            l[0].status,
            rated("A", Rating::Buy).status,
            "first answer counts"
        );
        assert_eq!(l[1].status, Status::Queued, "B still to do");

        let dir = std::env::temp_dir().join(format!("bmv-desk-torn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jsonl");
        append(&path, &q("A")).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"when\":\"cut").unwrap();
        assert_eq!(
            read_lines(&path).unwrap().len(),
            1,
            "a cut-off last line is ignored"
        );
        append(&path, &q("B")).unwrap();
        assert!(
            read_lines(&path).is_err(),
            "once followed by a full line, the torn one is an error, not silently lost"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn card(date: &str, rank: usize, rating: Rating, r: f64) -> Scorecard {
        Scorecard {
            date: date.into(),
            rank,
            symbol: "X".into(),
            rating,
            scored: Scored::Done {
                r,
                exit_date: date.into(),
            },
        }
    }

    fn date(i: usize) -> String {
        format!("2026-{:02}-{:02}", 1 + i / 28 % 12, 1 + i % 28)
    }

    #[test]
    fn tally_and_verdict() {
        let mut cards = Vec::new();
        for d in 0..60 {
            cards.push(card(&date(d), 1, Rating::Buy, 1.0));
            cards.push(card(&date(d), 2, Rating::Sell, -1.0));
        }
        let t = tally(&cards);
        assert_eq!(t.done, 120);
        assert_eq!(t.mean_all, Some(0.0));
        assert_eq!((t.weighted, t.d), (Some(1.0), Some(1.0)));
        assert!(
            verdict(&cards, TRIAL_NUMBER).passed(),
            "a perfect veto passes"
        );
        assert!(verdict(&cards, 12).checks[0].detail.contains("trial 12"));

        let coin: Vec<Scorecard> = (0..120)
            .map(|i| {
                let rating = if i % 2 == 0 {
                    Rating::Buy
                } else {
                    Rating::Sell
                };
                card(&date(i), 1, rating, if i % 3 == 0 { 1.0 } else { -0.5 })
            })
            .collect();
        assert!(
            !verdict(&coin, TRIAL_NUMBER).passed(),
            "a veto unrelated to outcomes fails"
        );
        assert_eq!(tally(&[]).mean_all, None);
    }

    #[test]
    fn the_sample_is_fixed_by_date_not_outcome() {
        let cards: Vec<Scorecard> = (0..200)
            .map(|i| card(&date(i / 2), i % 2 + 1, Rating::Buy, 0.0))
            .collect();
        let s = sample(&cards, Some("2026-01-01"), "2026-02-01").unwrap();
        assert_eq!(s.len(), TARGET_SIGNALS);
        assert_eq!(
            s.last().unwrap().date,
            date(74),
            "the first 150 by date and rank"
        );
        assert_eq!(
            sample(&cards[..100], Some("2026-01-01"), "2026-03-01"),
            None
        );
        let six = sample(&cards[..100], Some("2026-01-01"), "2026-07-05").unwrap();
        assert_eq!(
            six.len(),
            100,
            "after 6 months, everything from those months"
        );
    }

    /// Runs the real profile: the analyst can't read or write the project, only its run folder.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_sandbox_really_blocks_project_reads_and_writes() {
        // outside the temp folders the profile allows, like the real project in ~/Documents
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&target).unwrap();
        let base = std::fs::canonicalize(&target)
            .unwrap()
            .join(format!("sbx-test-{}", std::process::id()));
        // the real layout: project, sidecar and run folder all inside the user's home
        let home = base.join("home");
        let project = home.join("Documents/proj");
        let sidecar = project.join("side");
        let run = home.join("Library/Caches/run");
        for d in [&sidecar, &run, &home.join(".ssh")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(project.join("secret.env"), "ALPACA=x").unwrap();
        std::fs::write(home.join(".ssh/id"), "key").unwrap();
        std::fs::write(sidecar.join("run.py"), "print()").unwrap();
        let profile = sandbox_profile(&project, &sidecar, &home, &run);
        let sh = |cmd: String| {
            Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &profile, "/bin/sh", "-c", &cmd])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let d = |p: &Path| p.display().to_string();
        assert!(
            sh(format!("echo ok > {}/out && cat {}/out", d(&run), d(&run))),
            "run folder usable"
        );
        assert!(
            sh(format!("cat {}/run.py", d(&sidecar))),
            "sidecar readable"
        );
        assert!(
            !sh(format!("echo x > {}/evil", d(&project))),
            "project write blocked"
        );
        assert!(
            !sh(format!("echo x > {}/evil", d(&sidecar))),
            "sidecar write blocked"
        );
        assert!(
            !sh(format!("cat {}/secret.env", d(&project))),
            "project read blocked"
        );
        assert!(
            !sh(format!("cat {}/.ssh/id", d(&home))),
            "the rest of home unreadable"
        );
        assert!(
            !sh(format!(
                "echo x > /private/tmp/bmv-sbx-{}",
                std::process::id()
            )),
            "shared /tmp not writable"
        );
        assert!(!project.join("evil").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn progress_shows_counts_and_ratings_never_outcomes() {
        let p = Progress {
            signals: 0,
            rated: 0,
            failed: 0,
            unanalysed: 0,
            queued: 0,
            done: 0,
            open: 0,
            skipped: 0,
            spent_today: 0.0,
            spent_month: 0.0,
            cap_day: DAILY_BUDGET_USD,
            cap_month: MONTHLY_BUDGET_USD,
            next_run_cost: None,
            target: TARGET_SIGNALS,
            first: None,
            due_by: None,
            verdict: "not due",
            missed: vec![],
            night: None,
            cards: vec![],
        };
        let v = serde_json::to_value(&p).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        // the console shows this before the verdict: a new key must not carry R or win rates
        assert_eq!(
            keys,
            [
                "cap_day",
                "cap_month",
                "done",
                "due_by",
                "failed",
                "first",
                "missed",
                "next_run_cost",
                "night",
                "open",
                "queued",
                "rated",
                "signals",
                "skipped",
                "spent_month",
                "spent_today",
                "target",
                "unanalysed",
                "verdict"
            ]
        );
    }
}
