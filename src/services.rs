//! The console's read-only Services page: each moving part, whether it is up, and the command
//! that starts or fixes it. Probes only loopback, launchd and the Keychain's item names; it never
//! reads a secret value and never changes anything.

use crate::{desk, llm, watch};
use serde::Serialize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The launchd jobs in deploy/: label, log, schedule (Mac local time, as in the plist), and
/// whether the console may start it (`ui::RUNNABLE`).
const JOBS: [(&str, &str, &str); 4] = [
    (
        "com.bmv-trader.backstop",
        "data/backstop.log",
        "every 2 min",
    ),
    ("com.bmv-trader.desk", "data/desk/run.log", "weekdays 16:30"),
    ("com.bmv-trader.llm", "data/llm.log", "daily 05:30"),
    ("com.bmv-trader.notes", "data/notes/run.log", "daily 18:15"),
];
/// Keychain items by name; the bool is whether the item is required.
const KEYS: [(&str, bool); 4] = [
    ("ANTHROPIC_API_KEY", true),
    ("ANTHROPIC_WORKSPACE_ID", false),
    ("OMNIROUTE_API_KEY", true),
    ("OMNIROUTE_KEY_ID", true),
];
/// Where OmniRoute has been seen listening; the configured port is tried first.
const OMNIROUTE_PORTS: [u16; 3] = [20128, 20131, 20132];
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// Daily data older than this many days (weekend plus a holiday) is flagged.
const STALE_DAYS: i64 = 4;

#[derive(Debug, Serialize)]
pub struct Component {
    pub name: String,
    /// "ok", "warn", "down" or "unknown".
    pub status: &'static str,
    pub detail: String,
    /// A command to run in Terminal from the project folder; empty when nothing is needed.
    pub fix: String,
    /// The job the console's Run-now button starts (`ui::RUNNABLE`); empty when there is none or
    /// it is running now.
    pub run: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub now: i64,
    pub components: Vec<Component>,
}

fn c(name: &str, status: &'static str, detail: String, fix: &str) -> Component {
    Component {
        name: name.into(),
        status,
        // file contents can reach `detail`: keep it short
        detail: detail
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect(),
        fix: fix.into(),
        run: String::new(),
    }
}

/// Every check, slow ones included (Keychain, launchctl, loopback HTTP): call it off the request path.
pub fn check(home: &Path, console_port: u16) -> Report {
    let now = chrono::Utc::now().timestamp();
    let today = watch::today_et(now);
    let mut out = vec![
        engine(watch::heartbeat_age(home, now), in_session(now)),
        c(
            "Web console",
            "ok",
            format!("serving this page on 127.0.0.1:{console_port}"),
            "",
        ),
    ];
    let uid = uid();
    let agents = std::env::var_os("HOME").map(|h| Path::new(&h).join("Library/LaunchAgents"));
    for (label, log, schedule) in JOBS {
        let installed = agents
            .as_ref()
            .map(|a| a.join(format!("{label}.plist")).is_file());
        let print = uid.as_ref().and_then(|u| {
            run(
                "/bin/launchctl",
                &["print", &format!("gui/{u}/{label}")],
                &[],
                true,
            )
        });
        out.push(job(label, log, schedule, installed, print));
    }
    out.push(ollama());
    out.push(omniroute(llm::omniroute_port(home)));
    for (name, required) in KEYS {
        out.push(key(name, required, keychain_has(name)));
    }
    let python = home.join("sidecars/tradingagents/.venv/bin/python");
    out.push(if python.is_file() {
        c("Analyst sidecar venv", "ok", "present".into(), "")
    } else {
        c(
            "Analyst sidecar venv",
            "down",
            "sidecars/tradingagents/.venv/bin/python is missing".into(),
            "cd sidecars/tradingagents && uv venv --python 3.13 .venv && VIRTUAL_ENV=.venv uv pip install -r requirements.lock",
        )
    });
    out.extend(data(home, &today));
    Report {
        now,
        components: out,
    }
}

/// Weekday 09:30–16:00 New York time; holidays count as in session (a false "check" at worst).
fn in_session(ts: i64) -> bool {
    use chrono::{Datelike, Timelike};
    let Some(t) = chrono::DateTime::from_timestamp(ts, 0) else {
        return true;
    };
    let t = t.with_timezone(&chrono_tz::America::New_York);
    let minute = t.hour() * 60 + t.minute();
    t.weekday().num_days_from_monday() < 5 && (9 * 60 + 30..16 * 60).contains(&minute)
}

fn engine(age: Option<i64>, in_session: bool) -> Component {
    const FIX: &str = "caffeinate -i cargo run --release -- run";
    match age {
        Some(a) if (0..watch::HEARTBEAT_STALE_SECS).contains(&a) => {
            c("Engine (run)", "ok", format!("heartbeat {a} s ago"), "")
        }
        _ if !in_session => c("Engine (run)", "ok", "off (market closed)".into(), ""),
        Some(a) => c(
            "Engine (run)",
            "warn",
            format!(
                "last heartbeat {} min ago; normal outside a trading day",
                a / 60
            ),
            FIX,
        ),
        None => c("Engine (run)", "warn", "no heartbeat file yet".into(), FIX),
    }
}

/// `installed`: the plist is in ~/Library/LaunchAgents (None: no HOME). `print`: launchctl's
/// exit code and output (None: it couldn't run).
fn job(
    label: &str,
    log: &str,
    schedule: &str,
    installed: Option<bool>,
    print: Option<(i32, String)>,
) -> Component {
    let short = label.trim_start_matches("com.bmv-trader.");
    let name = format!("launchd {short}");
    let plist = format!("~/Library/LaunchAgents/{label}.plist");
    let bootstrap = format!("launchctl bootstrap gui/$(id -u) {plist}");
    let Some((code, text)) = print else {
        return c(&name, "unknown", "launchctl print didn't answer".into(), "");
    };
    let field = |key: &str| {
        text.lines()
            .map(str::trim)
            .find_map(|l| l.strip_prefix(key))
            .map(str::to_string)
    };
    match (installed, code == 0) {
        (Some(false), false) => c(
            &name,
            "down",
            "not installed".into(),
            &format!("cp deploy/{label}.plist ~/Library/LaunchAgents/ && {bootstrap}"),
        ),
        (_, false) => c(&name, "down", "installed but not loaded".into(), &bootstrap),
        (_, true) => {
            let state = field("state = ").unwrap_or_else(|| "state unknown".into());
            let exit = field("last exit code = ")
                .filter(|e| e != "(never exited)")
                .unwrap_or_else(|| "none yet".into());
            let failed = exit
                .split(':')
                .next()
                .and_then(|n| n.trim().parse::<i32>().ok())
                .is_some_and(|n| n != 0);
            let detail = format!("loaded, {state}, last exit {exit}; runs {schedule}");
            let mut out = if failed {
                c(&name, "warn", detail, &format!("tail -n 30 {log}"))
            } else if installed == Some(false) {
                c(
                    &name,
                    "warn",
                    format!("{detail}; its plist is gone from ~/Library/LaunchAgents"),
                    &format!("cp deploy/{label}.plist ~/Library/LaunchAgents/"),
                )
            } else {
                c(&name, "ok", detail, "")
            };
            if state != "running" && crate::ui::RUNNABLE.contains(&short) {
                out.run = short.into();
            }
            out
        }
    }
}

fn ollama() -> Component {
    match llm::ollama_has_model(HTTP_TIMEOUT) {
        Ok(d) => c("Ollama", "ok", d, ""),
        Err(e) if e.contains("missing") => c(
            "Ollama",
            "down",
            e,
            &format!("ollama pull {}", llm::LOCAL_MODEL),
        ),
        Err(e) => c("Ollama", "down", e, "brew services start ollama"),
    }
}

/// What an OpenAI-compatible GET /v1/models looks like, without a key: a model list, or a refusal
/// that asks for one. Anything else is some other service on that port.
fn api_answer(status: u16, body: &str) -> Option<&'static str> {
    match status {
        200 => serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .filter(|v| v.get("data").is_some_and(|d| d.is_array()))
            .map(|_| "open"),
        401 | 403 => Some("needs key"),
        _ => None,
    }
}

/// Never sends a key: a port that wants one is reported as "needs key".
fn probe_port(port: u16) -> Option<&'static str> {
    let r = llm::client_within(HTTP_TIMEOUT)
        .ok()?
        .get(format!("{}/v1/models", llm::omniroute_url(port)))
        .send()
        .ok()?;
    let status = r.status().as_u16();
    api_answer(status, &r.text().unwrap_or_default())
}

fn omniroute(configured: Result<u16, String>) -> Component {
    let mut ports: Vec<u16> = configured.iter().copied().collect();
    for p in OMNIROUTE_PORTS {
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    let seen: Vec<(u16, Option<&str>)> = ports.iter().map(|&p| (p, probe_port(p))).collect();
    omniroute_verdict(configured, &seen)
}

fn omniroute_verdict(configured: Result<u16, String>, seen: &[(u16, Option<&str>)]) -> Component {
    const NAME: &str = "OmniRoute API";
    let found = seen.iter().find_map(|(p, a)| a.map(|a| (*p, a)));
    let ports: Vec<String> = seen
        .iter()
        .map(|(p, a)| format!("{p}: {}", a.unwrap_or("no API")))
        .collect();
    let ports = ports.join(", ");
    let configured = match configured {
        Ok(p) => p,
        Err(e) => {
            return c(
                NAME,
                "down",
                e,
                "rm data/omniroute.json   # or write {\"port\": 20128}",
            );
        }
    };
    match found {
        Some((p, a)) if p == configured => c(
            NAME,
            "ok",
            format!("configured port {configured}, API on {p} ({a}). Probed {ports}"),
            "",
        ),
        Some((p, a)) => c(
            NAME,
            "warn",
            format!(
                "configured port {configured}, but the API answers on {p} ({a}). Probed {ports}"
            ),
            // a squatting process could answer too: only re-point if you started OmniRoute there
            &format!(
                "only if you started OmniRoute on port {p} yourself: echo '{{\"port\": {p}}}' > data/omniroute.json"
            ),
        ),
        None => c(
            NAME,
            "down",
            format!("configured port {configured}, no API found. Probed {ports}"),
            // never from the project folder: the CLI loads .env from its working directory
            "cd ~/.omniroute && omniroute serve --daemon",
        ),
    }
}

/// Whether the Keychain holds `account` under bmv-trader. Asks for the item's attributes only
/// (no `-w`) and throws stdout away: only the exit status is used, so no value can come back.
pub fn keychain_has(account: &str) -> bool {
    let home: Vec<(&str, String)> = std::env::var("HOME")
        .map(|h| vec![("HOME", h)])
        .unwrap_or_default();
    run(
        "/usr/bin/security",
        &[
            "find-generic-password",
            "-s",
            crate::config::KEYCHAIN_SERVICE,
            "-a",
            account,
        ],
        &home,
        false,
    )
    .is_some_and(|(code, _)| code == 0)
}

fn key(name: &str, required: bool, present: bool) -> Component {
    let label = format!("Keychain {name}");
    let fix = format!("security add-generic-password -s bmv-trader -a {name} -w");
    match (present, required) {
        (true, _) => c(&label, "ok", "present".into(), ""),
        (false, false) => c(
            &label,
            "ok",
            "not set; optional, only if Anthropic asks for a workspace".into(),
            "",
        ),
        (false, true) => c(
            &label,
            "down",
            "missing, or the keychain is locked".into(),
            &fix,
        ),
    }
}

fn days_old(date: &str, today: &str) -> Option<i64> {
    let d = |s: &str| chrono::NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok();
    Some((d(today)? - d(date)?).num_days())
}

fn fresh(name: &str, date: Result<String, String>, today: &str, fix: &str) -> Component {
    match date {
        Err(e) => c(name, "down", e, fix),
        Ok(d) => match days_old(&d, today) {
            Some(n) if n <= STALE_DAYS => c(name, "ok", format!("up to {}", &d[..10]), ""),
            Some(n) => c(
                name,
                "warn",
                format!("up to {}, {n} days old", &d[..10]),
                fix,
            ),
            None => c(name, "unknown", format!("unreadable date {d:?}"), fix),
        },
    }
}

fn data(home: &Path, today: &str) -> Vec<Component> {
    // no -k: that would kill a desk run already in progress
    const DESK: &str = "launchctl kickstart gui/$(id -u)/com.bmv-trader.desk";
    let range = std::fs::read(home.join("data/daily/sip-live/_range.json"))
        .map_err(|e| format!("no _range.json: {e}"))
        .and_then(|b| {
            serde_json::from_slice::<serde_json::Value>(&b)
                .ok()
                .and_then(|v| v.get("to")?.as_str().map(str::to_string))
                .ok_or_else(|| "_range.json has no `to`".to_string())
        });
    let signals = std::fs::read_dir(desk::desk_dir(home).join("signals"))
        .map_err(|e| format!("no signals folder: {e}"))
        .and_then(|d| {
            d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter_map(|n| n.strip_suffix(".json").map(str::to_string))
                .max()
                .ok_or_else(|| "no signals yet".to_string())
        });
    let health = match crate::history::read_json::<llm::Health>(&llm::health_path(home)) {
        Err(_) => c(
            "LLM health check",
            "down",
            "data/llm_health.json not written yet".into(),
            "target/release/bmv-trader llm-health",
        ),
        Ok(h) => {
            let failed: Vec<&str> = h
                .checks
                .iter()
                .filter(|c| !c.ok)
                .map(|c| c.name.as_str())
                .collect();
            let old = days_old(&h.when, today).is_none_or(|n| n > 2);
            let when = h.when.get(..16).unwrap_or(&h.when);
            if failed.is_empty() && !old {
                c("LLM health check", "ok", format!("all pass at {when}"), "")
            } else {
                let why = if failed.is_empty() {
                    "stale".to_string()
                } else {
                    format!("failing: {}", failed.join(", "))
                };
                c(
                    "LLM health check",
                    "warn",
                    format!("{why} (checked {when})"),
                    "target/release/bmv-trader llm-health",
                )
            }
        }
    };
    vec![
        fresh("Daily bars (sip-live)", range, today, DESK),
        fresh("Trial 10 signals", signals, today, DESK),
        health,
    ]
}

/// This user's numeric id, for launchd's `gui/<uid>` domain.
pub(crate) fn uid() -> Option<String> {
    run("/usr/bin/id", &["-u"], &[], true)
        .filter(|(code, u)| *code == 0 && !u.trim().is_empty())
        .map(|(_, u)| u.trim().to_string())
}

/// Runs a fixed system tool by absolute path, with a cleared environment (plus `env`) and a time
/// limit. Returns the exit code and stdout when `keep_stdout`, else stdout goes to /dev/null.
pub(crate) fn run(
    program: &str,
    args: &[&str],
    env: &[(&str, String)],
    keep_stdout: bool,
) -> Option<(i32, String)> {
    use std::io::Read;
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .stdout(if keep_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // read on a thread so a full pipe can't stall the child past the deadline
    let out = child.stdout.take().map(|o| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = o.take(1 << 20).read_to_string(&mut s);
            s
        })
    });
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code().unwrap_or(-1),
            Ok(None) if start.elapsed() < TOOL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = out.and_then(|t| t.join().ok()).unwrap_or_default();
    Some((code, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_probe_can_only_answer_yes_or_no() {
        // the signature is the guarantee: no String, so no secret value can travel out
        let probe: fn(&str) -> bool = keychain_has;
        assert!(!probe("bmv-trader-test-nonexistent-account"));
        assert_eq!(key("X", true, false).status, "down");
        assert_eq!(key("X", false, false).status, "ok");
        assert_eq!(key("X", true, true).detail, "present");
    }

    #[test]
    fn launchd_jobs_read_from_launchctl_print() {
        let (l, log, every) = JOBS[0];
        let loaded = "gui/501/x = {\n\tstate = not running\n\truns = 1\n\tlast exit code = 0\n\
                      \tendpoints = {\n\t\tstate = active\n\t}\n}";
        let ok = job(l, log, every, Some(true), Some((0, loaded.into())));
        assert_eq!(ok.status, "ok");
        assert_eq!(
            ok.detail,
            "loaded, not running, last exit 0; runs every 2 min"
        );
        assert_eq!(ok.run, "", "the backstop has no Run-now button");
        let failed = loaded.replace("code = 0", "code = 78: Function not implemented");
        let bad = job(l, log, every, Some(true), Some((0, failed)));
        assert_eq!(
            (bad.status, bad.fix.as_str()),
            ("warn", "tail -n 30 data/backstop.log")
        );
        let never = loaded.replace("last exit code = 0", "last exit code = (never exited)");
        let never = job(l, log, every, Some(true), Some((0, never)));
        assert_eq!(never.status, "ok");
        assert!(never.detail.contains("last exit none yet"));
        let (d, dlog, dev) = JOBS[1];
        assert_eq!(
            job(d, dlog, dev, Some(true), Some((0, loaded.into()))).run,
            "desk"
        );
        let running = loaded.replace("state = not running", "state = running");
        assert_eq!(job(d, dlog, dev, Some(true), Some((0, running))).run, "");
        let missing = job(l, log, every, Some(false), Some((113, String::new())));
        assert_eq!(missing.status, "down");
        assert!(
            missing
                .fix
                .starts_with("cp deploy/com.bmv-trader.backstop.plist")
        );
        let unloaded = job(l, log, every, Some(true), Some((113, String::new())));
        assert_eq!(unloaded.detail, "installed but not loaded");
        assert!(
            unloaded
                .fix
                .starts_with("launchctl bootstrap gui/$(id -u) ")
        );
        assert_eq!(job(l, log, every, Some(true), None).status, "unknown");
    }

    #[test]
    fn omniroute_is_found_by_its_api_not_by_an_open_port() {
        assert_eq!(
            api_answer(200, r#"{"object":"list","data":[]}"#),
            Some("open")
        );
        assert_eq!(api_answer(401, ""), Some("needs key"));
        assert_eq!(api_answer(200, "<html>dashboard</html>"), None);
        assert_eq!(api_answer(426, "{}"), None);
        assert_eq!(api_answer(404, ""), None);
        let same = omniroute_verdict(Ok(20128), &[(20128, Some("needs key")), (20131, None)]);
        assert_eq!((same.status, same.fix.as_str()), ("ok", ""));
        assert!(
            same.detail
                .contains("configured port 20128, API on 20128 (needs key)")
        );
        let moved = omniroute_verdict(Ok(20128), &[(20128, None), (20131, Some("open"))]);
        assert_eq!(moved.status, "warn");
        assert!(
            moved
                .fix
                .starts_with("only if you started OmniRoute on port 20131")
                && moved
                    .fix
                    .ends_with(r#"echo '{"port": 20131}' > data/omniroute.json"#)
        );
        assert_eq!(
            omniroute_verdict(Ok(20128), &[(20128, None)]).status,
            "down"
        );
        let bad = omniroute_verdict(Err("port 80 is below 1024".into()), &[]);
        assert_eq!(bad.status, "down");
    }

    #[test]
    fn data_ages_in_days() {
        assert_eq!(days_old("2026-11-04T03:48:14Z", "2026-11-03"), Some(-1));
        assert_eq!(days_old("2026-10-30", "2026-11-03"), Some(4));
        assert_eq!(days_old("nonsense", "2026-11-03"), None);
        let f = |d: &str| fresh("x", Ok(d.into()), "2026-11-03", "fix").status;
        assert_eq!(f("2026-10-30"), "ok");
        assert_eq!(f("2026-10-29"), "warn");
        assert_eq!(f("bad"), "unknown");
        assert_eq!(
            fresh("x", Err("none".into()), "2026-11-03", "f").status,
            "down"
        );
    }

    #[test]
    fn the_report_has_the_documented_shape() {
        let home = std::env::temp_dir().join(format!("bmv-services-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let v = serde_json::to_value(check(&home, 8765)).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        assert!(v["now"].is_i64());
        let comps = v["components"].as_array().unwrap();
        assert_eq!(comps.len(), 2 + JOBS.len() + 2 + KEYS.len() + 1 + 3);
        for comp in comps {
            let o = comp.as_object().unwrap();
            let mut keys: Vec<&str> = o.keys().map(String::as_str).collect();
            keys.sort();
            assert_eq!(keys, ["detail", "fix", "name", "run", "status"]);
            assert!(["ok", "warn", "down", "unknown"].contains(&o["status"].as_str().unwrap()));
        }
        assert_eq!(comps[1]["status"], "ok", "the console reports itself up");
        assert_eq!(
            comps[0]["status"],
            engine(None, in_session(v["now"].as_i64().unwrap())).status
        );
    }

    #[test]
    fn the_engine_is_only_missed_in_session() {
        // 2026-11-03 is a Tuesday; 15:00Z is 10:00 ET, 22:00Z is 17:00 ET
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
        assert!(in_session(at("2026-11-03T15:00:00Z")));
        assert!(!in_session(at("2026-11-03T22:00:00Z")));
        assert!(!in_session(at("2026-11-07T15:00:00Z")), "Saturday");
        assert_eq!(engine(None, true).status, "warn");
        assert_eq!(engine(Some(9999), false).detail, "off (market closed)");
    }

    #[test]
    fn schedules_match_the_plists() {
        for (label, _, schedule) in JOBS {
            let plist = std::fs::read_to_string(format!("deploy/{label}.plist")).unwrap();
            let int = |key: &str, n: &str| {
                plist.contains(&format!("<key>{key}</key><integer>{n}</integer>"))
            };
            let ok = match schedule.split_once(' ') {
                Some(("every", "2 min")) => {
                    plist.contains("<key>StartInterval</key>\n  <integer>120</integer>")
                }
                Some((days, hm)) => {
                    let (h, m) = hm.split_once(':').unwrap();
                    let h = h.trim_start_matches('0');
                    int("Hour", h)
                        && int("Minute", m)
                        && (days == "daily") == !plist.contains("<key>Weekday</key>")
                }
                None => false,
            };
            assert!(ok, "{label}: {schedule} doesn't match its plist");
        }
    }
}
