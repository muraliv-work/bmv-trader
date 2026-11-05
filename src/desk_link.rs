//! The engine's link to the Quant Desk (build step 4, DESIGN §4c). Off unless `BMV_DESK_LINK=on`.
//! On, a run trades only the strategy `BMV_DESK_STRATEGY` names, and only while the desk has it in
//! paper trading (or live, on a live account), under the desk's limits wherever they are tighter;
//! a halt the run hits is reported to the desk. The engine signs in as an Ops agent, whose token
//! is the Keychain item `bmv-desk` / `BMV_DESK_AGENT` (default `ops-engine`). Loopback only.

use crate::config::Mode;
use crate::money::Usd;
use crate::risk::Limits;
use serde_json::{Value, json};
use std::time::Duration;

pub struct Link {
    addr: String,
    token: String,
    strategy: i64,
    http: reqwest::blocking::Client,
}

/// The link's settings, from the environment; `None` when it is off.
fn settings(
    link: Option<&str>,
    strategy: Option<&str>,
    addr: Option<&str>,
) -> Result<Option<(String, i64)>, String> {
    match link {
        None | Some("off") => return Ok(None),
        Some("on") => {}
        Some(other) => return Err(format!("BMV_DESK_LINK is {other:?}; use on or off")),
    }
    let strategy = strategy
        .and_then(|s| s.trim().parse::<i64>().ok())
        .ok_or("BMV_DESK_LINK=on needs BMV_DESK_STRATEGY, the desk's strategy id")?;
    let addr = addr.unwrap_or("127.0.0.1:8790");
    if !(addr.starts_with("127.0.0.1:") || addr.starts_with("localhost:")) {
        return Err(format!(
            "DESK_ADDR {addr:?}: the desk is reached on loopback only"
        ));
    }
    Ok(Some((addr.to_string(), strategy)))
}

impl Link {
    /// The link, if `BMV_DESK_LINK=on`.
    pub fn from_env() -> Result<Option<Link>, String> {
        let var = |k| std::env::var(k).ok();
        let Some((addr, strategy)) = settings(
            var("BMV_DESK_LINK").as_deref(),
            var("BMV_DESK_STRATEGY").as_deref(),
            var("DESK_ADDR").as_deref(),
        )?
        else {
            return Ok(None);
        };
        let agent = var("BMV_DESK_AGENT").unwrap_or_else(|| "ops-engine".into());
        let out = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                "bmv-desk",
                "-a",
                &agent,
                "-w",
            ])
            .output()
            .map_err(|e| format!("Keychain: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "no desk token for {agent}: add the Keychain item bmv-desk/{agent}"
            ));
        }
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Some(Link {
            addr,
            token: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            strategy,
            http,
        }))
    }

    fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut req = self
            .http
            .request(method, format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().map_err(|e| format!("desk {path}: {e}"))?;
        let ok = resp.status().is_success();
        let v: Value = resp.json().map_err(|e| format!("desk {path}: {e}"))?;
        if ok {
            Ok(v)
        } else {
            Err(format!(
                "desk {path}: {}",
                v["error"].as_str().unwrap_or("refused")
            ))
        }
    }

    /// The desk's go-ahead for this run, and the limits to run under.
    pub fn approve(&self, mode: Mode, limits: Limits, equity: Usd) -> Result<Limits, String> {
        let me = self.call(reqwest::Method::GET, "/v1/whoami", None)?;
        let all = self.call(reqwest::Method::GET, "/v1/strategies", None)?;
        let lim = self.call(
            reqwest::Method::GET,
            &format!("/v1/strategies/{}/limits", self.strategy),
            None,
        )?;
        decide(mode, self.strategy, &me, &all, &lim, limits, equity)
    }

    /// Tells the desk this strategy stopped on a halt; the desk records it and decides who resumes.
    pub fn report_halt(&self, why: &str) -> Result<(), String> {
        eprintln!("desk: reporting the halt ({why})");
        self.call(
            reqwest::Method::POST,
            &format!("/v1/strategies/{}/halt", self.strategy),
            Some(json!({"reason": "operational"})),
        )
        .map(|_| ())
    }
}

/// The console's read-only desk login (S47): Keychain `bmv-desk` / `console-1`, role `console`,
/// which the desk refuses every write.
pub struct Reader {
    addr: std::net::SocketAddr,
    token: String,
    http: reqwest::blocking::Client,
}

impl Reader {
    /// The login, checked with the desk: its token must be a `console` one, so a token filed under
    /// the wrong role is refused here instead of quietly reading with more rights.
    pub fn from_keychain() -> Result<Reader, String> {
        let var = std::env::var("DESK_ADDR").unwrap_or_else(|_| "127.0.0.1:8790".into());
        let addr = loopback(&var)?;
        let token = crate::config::keychain_in("bmv-desk", "console-1")?.ok_or(
            "no console token in the Keychain (bmv-desk / console-1); see deploy/README.md, Quant Desk console",
        )?;
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let r = Reader { addr, token, http };
        let me = r.get("/v1/whoami")?;
        if me["role"] != "console" {
            return Err(format!(
                "the token at bmv-desk / console-1 belongs to {} (role {}), not a console login; reissue it per deploy/README.md",
                me["agent"].as_str().unwrap_or("?"),
                me["role"].as_str().unwrap_or("?")
            ));
        }
        Ok(r)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let resp = self
            .http
            .get(format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| format!("desk {path}: {}", e.without_url()))?;
        let status = resp.status();
        let v: Value = resp
            .json()
            .map_err(|e| format!("desk {path}: {status}, {}", e.without_url()))?;
        if status.is_success() {
            Ok(v)
        } else {
            Err(format!(
                "desk {path}: {status}, {}",
                v["error"].as_str().unwrap_or("refused")
            ))
        }
    }

    /// Everything the console's desk page shows, in one reading.
    pub fn snapshot(&self) -> Result<Value, String> {
        let started = std::time::Instant::now();
        snapshot(&|p| self.get(p), &|| started.elapsed() > READING_LIMIT)
    }
}

/// One reading gives up after this, so a hung desk can't hold the page's reading for one timeout
/// per strategy.
const READING_LIMIT: Duration = Duration::from_secs(30);

/// `DESK_ADDR` as a loopback socket address: a parsed address, not a prefix, so nothing like
/// `127.0.0.1:80@elsewhere` gets the token.
fn loopback(s: &str) -> Result<std::net::SocketAddr, String> {
    s.parse::<std::net::SocketAddr>()
        .ok()
        .filter(|a| a.ip().is_loopback())
        .ok_or_else(|| format!("DESK_ADDR {s:?}: the desk is reached on a loopback ip:port only"))
}

/// The desk page's reading, from the desk's GET routes. A broken audit chain or one strategy's
/// unreadable evidence is shown on its own line, not as a failed reading; running out of time is.
fn snapshot(
    get: &dyn Fn(&str) -> Result<Value, String>,
    out_of_time: &dyn Fn() -> bool,
) -> Result<Value, String> {
    let strategies = get("/v1/strategies")?;
    let mut evidence = serde_json::Map::new();
    for s in strategies
        .as_array()
        .ok_or("desk: strategies isn't a list")?
    {
        let id = s["id"].as_i64().ok_or("desk: a strategy has no id")?;
        if out_of_time() {
            return Err(format!(
                "the desk reading took over {} s; showing the last one",
                READING_LIMIT.as_secs()
            ));
        }
        let e =
            get(&format!("/v1/strategies/{id}/evidence")).unwrap_or_else(|e| json!({"error": e}));
        evidence.insert(id.to_string(), e);
    }
    Ok(json!({
        "strategies": strategies,
        "evidence": evidence,
        "requests": get("/v1/owner/requests")?,
        "budgets": get("/v1/budgets")?,
        "audit": get("/v1/audit/verify").unwrap_or_else(|e| json!({"error": e})),
    }))
}

/// The go-ahead, from the desk's replies: the engine's agent must be Ops, the strategy must be in
/// the state that allows trading in this mode, and every desk limit must be one the engine enforces.
fn decide(
    mode: Mode,
    strategy: i64,
    me: &Value,
    all: &Value,
    lim: &Value,
    mut limits: Limits,
    equity: Usd,
) -> Result<Limits, String> {
    if me["role"] != "ops" {
        return Err(format!(
            "the desk token belongs to {} ({}), not an Ops agent",
            me["agent"], me["role"]
        ));
    }
    let row = all
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"].as_i64() == Some(strategy)))
        .ok_or_else(|| format!("the desk has no strategy {strategy}"))?;
    let state = row["state"].as_str().unwrap_or("");
    let allowed: &[&str] = match mode {
        Mode::Paper => &["paper_trading"],
        Mode::Live => &["live", "live_full_size"],
    };
    if !allowed.contains(&state) {
        return Err(format!(
            "the desk has strategy {strategy} ({}) in {state}, which doesn't allow {mode:?} trading",
            row["name"]
        ));
    }
    for l in lim.as_array().ok_or("the desk sent bad limits")? {
        let (Some(name), Some(value)) = (l["name"].as_str(), l["value"].as_str()) else {
            return Err("the desk sent a bad limit".into());
        };
        limits = limits.tightened(name, value, equity)?;
    }
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_unless_on_and_loopback_only() {
        assert_eq!(settings(None, None, None), Ok(None));
        assert_eq!(settings(Some("off"), Some("3"), None), Ok(None));
        assert!(settings(Some("yes"), Some("3"), None).is_err());
        assert!(settings(Some("on"), None, None).is_err());
        assert_eq!(
            settings(Some("on"), Some("3"), None),
            Ok(Some(("127.0.0.1:8790".into(), 3)))
        );
        assert!(settings(Some("on"), Some("3"), Some("10.0.0.5:8790")).is_err());
    }

    #[test]
    fn trades_only_a_strategy_the_desk_has_in_paper_trading() {
        let eq = Usd::from_cents(2_500_000);
        let ops = json!({"agent": "ops-engine", "role": "ops"});
        let rows = |state: &str| json!([{"id": 3, "name": "orb", "state": state, "version": 1}]);
        let none = json!([]);
        let base = || Limits::for_mode(Mode::Paper);
        let go = decide(
            Mode::Paper,
            3,
            &ops,
            &rows("paper_trading"),
            &none,
            base(),
            eq,
        );
        assert_eq!(go, Ok(base()));
        assert!(decide(Mode::Paper, 3, &ops, &rows("validated"), &none, base(), eq).is_err());
        assert!(
            decide(
                Mode::Paper,
                4,
                &ops,
                &rows("paper_trading"),
                &none,
                base(),
                eq
            )
            .is_err()
        );
        assert!(
            decide(
                Mode::Live,
                3,
                &ops,
                &rows("paper_trading"),
                &none,
                base(),
                eq
            )
            .is_err()
        );
        let risk = json!({"agent": "risk-1", "role": "risk_manager"});
        assert!(
            decide(
                Mode::Paper,
                3,
                &risk,
                &rows("paper_trading"),
                &none,
                base(),
                eq
            )
            .is_err()
        );

        let tight = json!([{"name": "max_trades_per_day", "value": "2"}]);
        let t = decide(
            Mode::Paper,
            3,
            &ops,
            &rows("paper_trading"),
            &tight,
            base(),
            eq,
        )
        .unwrap();
        assert_ne!(t, base());
        let unknown = json!([{"name": "max_correlated_exposure_pct", "value": "5"}]);
        assert!(
            decide(
                Mode::Paper,
                3,
                &ops,
                &rows("paper_trading"),
                &unknown,
                base(),
                eq
            )
            .is_err()
        );
    }

    #[test]
    fn the_console_snapshot_shows_a_broken_part_without_losing_the_rest() {
        let get = |p: &str| -> Result<Value, String> {
            match p {
                "/v1/strategies" => {
                    Ok(json!([{"id": 1, "name": "drift", "state": "idea", "version": 1}]))
                }
                "/v1/strategies/1/evidence" => Err("desk: no evidence".into()),
                "/v1/owner/requests" | "/v1/budgets" => Ok(json!([])),
                "/v1/audit/verify" => Err("audit chain broken at 7".into()),
                _ => Err(format!("unexpected {p}")),
            }
        };
        let s = snapshot(&get, &|| false).unwrap();
        assert_eq!(s["strategies"][0]["name"], "drift");
        assert_eq!(s["evidence"]["1"]["error"], "desk: no evidence");
        assert_eq!(s["audit"]["error"], "audit chain broken at 7");
        let down = |_: &str| -> Result<Value, String> { Err("can't reach the desk".into()) };
        assert!(
            snapshot(&down, &|| false).is_err(),
            "no strategies, no reading"
        );
        assert!(
            snapshot(&get, &|| true).unwrap_err().contains("took over"),
            "a slow desk ends the reading"
        );
        assert!(loopback("127.0.0.1:8790").is_ok() && loopback("[::1]:8790").is_ok());
        for bad in [
            "10.0.0.2:8790",
            "127.0.0.1:80@evil",
            "localhost:8790",
            "127.0.0.1",
        ] {
            assert!(loopback(bad).is_err(), "{bad}");
        }
    }
}
