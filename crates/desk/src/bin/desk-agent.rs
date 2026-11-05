//! `desk-agent AGENTS AGENT SECRETS`: one heartbeat of one desk agent (DESIGN §4c, S36, S38, S39).
//! Paperclip's `process` adapter starts it; it runs `desk::agent::run` and prints what the agent
//! said. `desk-agent --list AGENTS` checks the files and prints each agent's setup as JSON, for the
//! setup scripts.
//!
//! - AGENTS: the owner's agents file (`desk::agent::Agents`), read with `models.json` from the same
//!   folder, which names each agent's provider, model and budget. Neither may be writable by the
//!   agent: owned by another user, and not group- or world-writable.
//! - SECRETS: `{"desk_token": "...", "anthropic_key": "...", "omniroute_key": "..."}`, owned by the
//!   agent's user and readable by it only. The run uses the key for the provider its `claude`
//!   switch picks; holding both lets the switch flip without new keys (S41).
//!
//! Each call's usage is reported to Paperclip when its run variables are set.

use desk::agent::{self, Agents, Provider, Usage};
use desk::api::{self, DEFAULT_ADDR};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::process::ExitCode;
use std::time::Duration;

const ANTHROPIC: &str = "https://api.anthropic.com/v1/messages";
const OMNIROUTE: &str = "http://127.0.0.1:20128/v1/chat/completions";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Secrets {
    desk_token: String,
    anthropic_key: Option<String>,
    omniroute_key: Option<String>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, agents] if flag == "--list" => list(agents),
        [agents, agent, secrets] => heartbeat(agents, agent, secrets),
        _ => Err("usage: desk-agent AGENTS AGENT SECRETS | desk-agent --list AGENTS".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("desk-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Reads a file once, checking the opened file itself (no gap between check and read). The path
/// must be a regular file, not a link, and the file opened must be the one the path names.
fn read(
    path: &str,
    check: impl Fn(&std::fs::Metadata) -> Result<(), String>,
) -> Result<(String, u32), String> {
    let link = std::fs::symlink_metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !link.file_type().is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    let mut f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let meta = f.metadata().map_err(|e| format!("{path}: {e}"))?;
    if (meta.dev(), meta.ino()) != (link.dev(), link.ino()) {
        return Err(format!("{path}: changed while it was opened"));
    }
    check(&meta).map_err(|e| format!("{path}: {e}"))?;
    let mut text = String::new();
    f.read_to_string(&mut text)
        .map_err(|e| format!("{path}: {e}"))?;
    Ok((text, meta.uid()))
}

/// True for `127.0.0.1` or `localhost` over plain HTTP, with no user info.
fn loopback(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
}

/// The models file that goes with an agents file: `models.json` in the same folder.
fn models_path(agents: &str) -> String {
    std::path::Path::new(agents)
        .with_file_name("models.json")
        .to_string_lossy()
        .into_owned()
}

/// Each agent's setup, as the setup scripts need it; no prompts.
fn list(path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let models = models_path(path);
    let mtext = std::fs::read_to_string(&models).map_err(|e| format!("{models}: {e}"))?;
    let a = Agents::parse(&text, &mtext)?;
    let rows: Vec<Value> = a
        .agents
        .iter()
        .map(|(id, s)| {
            json!({"id": id, "role": s.role, "runs_as": s.runs_as, "provider": s.provider,
                   "model": s.model, "budget_usd": s.budget_usd, "schedule": s.schedule,
                   "keys": s.keys_path(id), "signs": s.signs()})
        })
        .collect();
    println!("{}", Value::Array(rows));
    Ok(())
}

fn heartbeat(agents: &str, agent: &str, secrets: &str) -> Result<(), String> {
    let (text, me) = read(secrets, |m| {
        if m.mode() & 0o077 != 0 {
            return Err(format!(
                "readable by others (mode {:o}); chmod 600 it",
                m.mode() & 0o777
            ));
        }
        Ok(())
    })?;
    let s: Secrets = serde_json::from_str(&text).map_err(|_| {
        format!("{secrets} isn't {{\"desk_token\", \"anthropic_key\" or \"omniroute_key\"}}")
    })?;
    let owners_only = |m: &std::fs::Metadata| {
        if m.mode() & 0o022 != 0 {
            return Err("writable by others; it must be the owner's alone".into());
        }
        Ok(())
    };
    let models = models_path(agents);
    let (text, owner) = read(agents, owners_only)?;
    let (mtext, mowner) = read(&models, owners_only)?;
    // the secrets file belongs to the agent's user; the agents and models files must not
    for (path, o) in [(agents, owner), (models.as_str(), mowner)] {
        if o == me {
            return Err(format!(
                "{path} belongs to the agent's own user, which could rewrite it"
            ));
        }
    }
    let mut c = Agents::parse(&text, &mtext)?.get(agent)?.charter(agent)?;
    // the model has no clock: each heartbeat is told when it is
    c.task = format!(
        "It is {} UTC.\n\n{}",
        chrono::Utc::now().format("%A %Y-%m-%d %H:%M"),
        c.task
    );
    let key = match c.provider {
        Provider::Anthropic => s
            .anthropic_key
            .as_deref()
            .ok_or("no anthropic_key in the secrets")?,
        Provider::Omniroute => s
            .omniroute_key
            .as_deref()
            .ok_or("no omniroute_key in the secrets")?,
    };

    let addr = std::env::var("DESK_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.into());
    if !addr.starts_with("127.0.0.1:") {
        return Err(format!("DESK_ADDR {addr}: the desk is on 127.0.0.1 only"));
    }
    let desk = |method: &str, path: &str, body: Option<&Value>| {
        api::call(&addr, method, path, Some(&s.desk_token), body)
    };
    // no redirects (a custom key header would follow one) and no proxies (plain-HTTP keys)
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let post = |body: &Value| -> Result<Value, String> {
        let req = match c.provider {
            Provider::Anthropic => http
                .post(ANTHROPIC)
                .header("x-api-key", key)
                .header("anthropic-version", "2026-10-21"),
            Provider::Omniroute => http.post(OMNIROUTE).bearer_auth(key),
        };
        let resp = req
            .json(body)
            .send()
            .map_err(|e| format!("model call: {}", e.without_url()))?;
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| format!("model reply: {}", e.without_url()))?;
        if !status.is_success() {
            let v: Value = serde_json::from_str(&text).unwrap_or_default();
            let why: String = v["error"]["message"]
                .as_str()
                .unwrap_or("refused")
                .chars()
                .take(300)
                .collect();
            return Err(format!(
                "model call: {status}: {}",
                why.replace(key, "[key]")
            ));
        }
        serde_json::from_str(&text).map_err(|_| format!("model reply ({status}) isn't JSON"))
    };
    let paperclip = Paperclip::from_env(&http);
    let mut report = |u: &Usage| {
        if let Some(p) = &paperclip
            && let Err(e) = p.cost(c.provider, u)
        {
            // the desk's books are the authority; Paperclip's are a mirror
            eprintln!("desk-agent: Paperclip didn't take the cost report: {e}");
        }
    };
    let out = agent::run(&c, &desk, &post, &mut report)?;
    println!("{}", out.text);
    eprintln!(
        "desk-agent: {} in {} turn(s), ${}, tools: {}",
        c.agent,
        out.turns,
        out.usd.round_dp(4),
        if out.tools.is_empty() {
            "none".into()
        } else {
            out.tools.join(", ")
        }
    );
    Ok(())
}

/// The run's link back to Paperclip, from the variables its `process` adapter sets.
struct Paperclip<'a> {
    http: &'a reqwest::blocking::Client,
    url: reqwest::Url,
    key: String,
    company: String,
    agent: String,
    run: String,
}

impl<'a> Paperclip<'a> {
    fn from_env(http: &'a reqwest::blocking::Client) -> Option<Self> {
        let v = |k| std::env::var(k).ok().filter(|s: &String| !s.is_empty());
        let url = reqwest::Url::parse(&v("PAPERCLIP_API_URL")?).ok()?;
        // Paperclip is on this Mac; its run key goes nowhere else
        if !loopback(&url) {
            eprintln!("desk-agent: PAPERCLIP_API_URL isn't loopback; not reporting");
            return None;
        }
        Some(Paperclip {
            http,
            url,
            key: v("PAPERCLIP_API_KEY")?,
            company: v("PAPERCLIP_COMPANY_ID")?,
            agent: v("PAPERCLIP_AGENT_ID")?,
            run: v("PAPERCLIP_RUN_ID")?,
        })
    }

    fn cost(&self, provider: Provider, u: &Usage) -> Result<(), String> {
        let cents = (u.usd * rust_decimal::Decimal::from(100)).round_dp(6);
        let body = json!({
            "agentId": self.agent,
            "heartbeatRunId": self.run,
            "provider": match provider { Provider::Anthropic => "anthropic", Provider::Omniroute => "omniroute" },
            "billingType": match provider { Provider::Anthropic => "metered_api", Provider::Omniroute => "fixed" },
            "model": if u.model.is_empty() { "unknown" } else { &u.model },
            "inputTokens": u.input,
            "outputTokens": u.output,
            "costCents": cents.to_string(),
            "occurredAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        });
        let mut url = self.url.clone();
        url.path_segments_mut()
            .map_err(|_| "bad PAPERCLIP_API_URL")?
            .pop_if_empty()
            .extend(["api", "companies", &self.company, "cost-events"]);
        let r = self
            .http
            .post(url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .map_err(|e| e.without_url().to_string())?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(r.status().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::loopback;

    #[test]
    fn only_plain_loopback_urls_get_the_run_key() {
        let ok = |s: &str| loopback(&reqwest::Url::parse(s).unwrap());
        assert!(ok("http://127.0.0.1:3100"));
        assert!(ok("http://localhost:3100/"));
        assert!(!ok("http://127.0.0.1:80@evil.example/"));
        assert!(!ok("http://localhost:x@evil.example/"));
        assert!(!ok("https://127.0.0.1:3100"));
        assert!(!ok("http://127.0.0.1.evil.example:3100"));
    }
}
