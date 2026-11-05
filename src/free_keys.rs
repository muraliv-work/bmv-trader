//! Free cloud models' keys (DECISION S42): the lifecycle of the provider keys behind OmniRoute's
//! `bmv-free` route. Only providers with an official key API (OpenRouter) are created and rotated
//! here; the others are pasted in once with `scripts/free-key-add.sh` and tracked for expiry.
//!
//! - **Values:** a key's value lives only in OmniRoute's encrypted store. It is in this process's
//!   memory between creation and hand-over, goes to OmniRoute in one child's environment
//!   (`--from-env`, never argv), and is never logged.
//! - **Records:** `data/free_keys.jsonl` holds metadata only, one event per line, append-only; a key's
//!   state is its latest event.
//! - **Rotation:** a new key is created with a credit cap and an expiry, handed to OmniRoute and
//!   tested there. Only then is the old one deleted; on any failure the new one is deleted and the
//!   old one stays. Rotation is for security: extra keys don't raise a provider's limits.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const OPENROUTER_KEYS: &str = "https://openrouter.ai/api/v1/keys";
/// Every key this job creates is named with this prefix, so cleanup never touches anyone else's.
const PREFIX: &str = "bmv-free-";
/// The child environment variable that carries a new key to `omniroute providers rotate`.
const KEY_VAR: &str = "BMV_NEW_KEY";
/// A created key outlives its rotation date by this much, then expires on its own.
const GRACE_DAYS: i64 = 7;

/// The `free_keys` section of `deploy/models.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub rotate_every_hours: i64,
    pub providers: BTreeMap<String, Provider>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// The OmniRoute provider connection (`omniroute providers list`) this key backs.
    pub connection: String,
    /// True only for a provider with a key API: create and rotate it here.
    pub auto: bool,
    /// For a pasted key: the days until it should be replaced by hand.
    #[serde(default)]
    pub expires_days: Option<i64>,
    /// For an auto key: its credit cap in USD. 0 means it can only use free models.
    #[serde(default)]
    pub credit_limit_usd: Option<f64>,
}

#[derive(Deserialize)]
struct ModelsFile {
    free_keys: Config,
}

impl Config {
    pub fn parse(models_json: &str) -> Result<Config, String> {
        let c = serde_json::from_str::<ModelsFile>(models_json)
            .map_err(|e| format!("models file, free_keys: {e}"))?
            .free_keys;
        if !(24..=24 * 90).contains(&c.rotate_every_hours) {
            return Err("free_keys: rotate_every_hours must be 24 to 2160 (1 to 90 days)".into());
        }
        for (name, p) in &c.providers {
            let at = |e: &str| format!("free_keys, {name}: {e}");
            if p.connection.is_empty()
                || !p
                    .connection
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(at("connection must be letters, digits, - or _"));
            }
            match (p.auto, name.as_str()) {
                (true, "openrouter") => {}
                (true, _) => return Err(at("only openrouter has a key API; set auto to false")),
                (false, _) if p.expires_days.is_none_or(|d| !(1..=365).contains(&d)) => {
                    return Err(at("a pasted key needs expires_days, 1 to 365"));
                }
                (false, _) => {}
            }
            if p.credit_limit_usd
                .is_some_and(|l| !l.is_finite() || l < 0.0)
            {
                return Err(at("credit_limit_usd must be a number >= 0"));
            }
        }
        Ok(c)
    }

    pub fn load(home: &Path) -> Result<Config, String> {
        let path = crate::llm::models_path(home);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Config::parse(&text)
    }
}

/// What happened to a key.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum What {
    /// Made at the provider, not yet in OmniRoute.
    Created,
    /// In OmniRoute and passing its test: the one in use.
    Active,
    /// A pasted key, recorded with its expiry.
    Recorded,
    /// Deleted at the provider (or, for a pasted key, replaced).
    Retired,
    /// A step failed; the note says which.
    Failed,
}

/// One line of `data/free_keys.jsonl`. Never holds a key's value.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// RFC 3339, UTC.
    pub when: String,
    pub provider: String,
    pub connection: String,
    /// The provider's id for the key (OpenRouter's hash), or `pasted-<when>` for a pasted one.
    pub key_id: String,
    pub what: What,
    /// When the key expires (RFC 3339), if known.
    pub expires: Option<String>,
    pub note: String,
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("data").join("free_keys.jsonl")
}

pub fn read_log(path: &Path) -> Result<Vec<Event>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(n, l)| {
            serde_json::from_str(l).map_err(|e| format!("{} line {}: {e}", path.display(), n + 1))
        })
        .collect()
}

/// Each key's latest event, in log order (the log is append-only, so position is time).
fn latest<'a>(events: &'a [Event], provider: &str) -> Vec<(usize, &'a Event)> {
    let mut last: BTreeMap<&str, (usize, &Event)> = BTreeMap::new();
    for (i, e) in events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.provider == provider)
    {
        last.insert(e.key_id.as_str(), (i, e));
    }
    let mut v: Vec<_> = last.into_values().collect();
    v.sort_by_key(|(i, _)| *i);
    v
}

/// The key a provider is using now: the most recent one whose latest event is Active or Recorded.
pub fn current<'a>(events: &'a [Event], provider: &str) -> Option<&'a Event> {
    current_at(events, provider).map(|(_, e)| e)
}

fn current_at<'a>(events: &'a [Event], provider: &str) -> Option<(usize, &'a Event)> {
    latest(events, provider)
        .into_iter()
        .rfind(|(_, e)| matches!(e.what, What::Active | What::Recorded))
}

/// Keys whose handover outcome was never recorded (Created was their last event). OmniRoute may
/// hold one, so cleanup never deletes them; the next pass rotates again, which replaces whatever
/// OmniRoute holds and makes them safe to delete.
fn pending<'a>(events: &'a [Event], provider: &str) -> Vec<&'a str> {
    latest(events, provider)
        .into_iter()
        .filter(|(_, e)| e.what == What::Created)
        .map(|(_, e)| e.key_id.as_str())
        .collect()
}

/// Every key this job has created for `provider`, by its log.
fn created_here<'a>(events: &'a [Event], provider: &str) -> std::collections::BTreeSet<&'a str> {
    events
        .iter()
        .filter(|e| e.provider == provider && e.what == What::Created)
        .map(|e| e.key_id.as_str())
        .collect()
}

fn since(when: &str, now: chrono::DateTime<chrono::Utc>) -> Option<chrono::Duration> {
    let t = chrono::DateTime::parse_from_rfc3339(when).ok()?;
    Some(now - t.with_timezone(&chrono::Utc))
}

fn stamp(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A new key: its hash (the provider's id for it) and its value.
pub struct NewKey {
    pub hash: String,
    pub value: String,
}

/// One key in the provider's list.
pub struct Listed {
    pub hash: String,
    pub name: String,
    pub disabled: bool,
}

/// OpenRouter's key API, as calls the job makes. Real ones hold the management key; tests fake them.
pub struct KeyApi<'a> {
    /// Creates a key from its name, credit limit and expiry.
    pub create: &'a dyn Fn(&str, f64, &str) -> Result<NewKey, String>,
    /// Deletes a key by hash; a key that is already gone counts as deleted.
    pub delete: &'a dyn Fn(&str) -> Result<(), String>,
    pub list: &'a dyn Fn() -> Result<Vec<Listed>, String>,
}

/// OmniRoute, as calls the job makes.
pub struct OmniRoute<'a> {
    /// Gives a connection a new key; OmniRoute tests it before reporting success.
    pub handover: &'a dyn Fn(&str, &str) -> Result<(), String>,
    /// Tests a connection with the key it holds now.
    pub test: &'a dyn Fn(&str) -> Result<(), String>,
}

/// One pass at a time, for the job and for `--record`: an exclusive lock beside the log.
pub fn lock(log: &Path) -> Result<std::fs::File, String> {
    let path = log.with_extension("lock");
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    f.try_lock().map_err(|_| {
        format!(
            "another free-keys pass is running ({} is locked)",
            path.display()
        )
    })?;
    Ok(f)
}

/// One lifecycle pass for OpenRouter, under the lock: rotate if due (or if a key is pending),
/// then delete the keys this job made that it no longer uses. Every failure is logged as Failed,
/// so the health check shows it. Returns what it did.
pub fn run_openrouter(
    cfg: &Config,
    log: &Path,
    now: chrono::DateTime<chrono::Utc>,
    api: &KeyApi,
    omni: &OmniRoute,
) -> Result<String, String> {
    let Some(p) = cfg.providers.get("openrouter").filter(|p| p.auto) else {
        return Ok("openrouter isn't set to auto; nothing to rotate".into());
    };
    let _held = lock(log)?;
    let event = |key_id: &str, what, expires: Option<String>, note: String| Event {
        when: stamp(now),
        provider: "openrouter".into(),
        connection: p.connection.clone(),
        key_id: key_id.into(),
        what,
        expires,
        note,
    };
    let fail = |key_id: &str, note: String| -> Result<(), String> {
        crate::desk::append(log, &event(key_id, What::Failed, None, note))
    };
    let events = read_log(log)?;
    let before = current(&events, "openrouter").cloned();
    let unsure = pending(&events, "openrouter");
    // a few minutes' slack, so a job run just short of the interval doesn't slip a whole run
    let every = chrono::Duration::hours(cfg.rotate_every_hours) - chrono::Duration::minutes(30);
    let due = !unsure.is_empty()
        || before
            .as_ref()
            .is_none_or(|e| since(&e.when, now).is_none_or(|d| d >= every));
    let mut did = Vec::new();
    let mut active = before.as_ref().map(|e| e.key_id.clone());
    if due {
        let expires = stamp(
            now + chrono::Duration::hours(cfg.rotate_every_hours)
                + chrono::Duration::days(GRACE_DAYS),
        );
        let name = format!("{PREFIX}{}", now.format("%Y%m%d-%H%M"));
        let new = match (api.create)(&name, p.credit_limit_usd.unwrap_or(0.0), &expires) {
            Ok(k) => k,
            Err(e) => {
                fail("none", format!("creating a key failed: {e}"))?;
                return Err(format!("couldn't create a new OpenRouter key: {e}"));
            }
        };
        crate::desk::append(
            log,
            &event(&new.hash, What::Created, Some(expires.clone()), name),
        )?;
        // OmniRoute's output stays out of the log: it could echo part of the key
        if (omni.handover)(&p.connection, &new.value).is_err() {
            let undone = (api.delete)(&new.hash).is_ok();
            let still_works = (omni.test)(&p.connection).is_ok();
            fail(
                &new.hash,
                format!(
                    "OmniRoute refused the new key; it was {}; the connection {}",
                    if undone { "deleted" } else { "NOT deleted" },
                    if still_works {
                        "still works"
                    } else {
                        "FAILS its test"
                    }
                ),
            )?;
            return Err(format!(
                "OmniRoute refused the new OpenRouter key (see `omniroute providers test {}`). The new key \
                 was {}, and the connection {}.",
                p.connection,
                if undone {
                    "deleted"
                } else {
                    "NOT deleted: delete it at openrouter.ai"
                },
                if still_works {
                    "still works on its old key"
                } else {
                    "now FAILS: check it in OmniRoute's dashboard"
                }
            ));
        }
        crate::desk::append(
            log,
            &event(
                &new.hash,
                What::Active,
                Some(expires),
                "in OmniRoute, test passed".into(),
            ),
        )?;
        did.push("rotated".to_string());
        active = Some(new.hash);
    }
    // only keys this job made (by its own log, and by name), never one in use or still pending
    let events = read_log(log)?;
    let ours = created_here(&events, "openrouter");
    let unsure = pending(&events, "openrouter");
    let listed = match (api.list)() {
        Ok(l) => l,
        Err(e) => {
            fail("none", format!("listing keys failed: {e}"))?;
            return Err(format!("couldn't list OpenRouter keys: {e}"));
        }
    };
    let (mut retired, mut problems) = (0, Vec::new());
    for k in listed {
        if !k.name.starts_with(PREFIX)
            || !ours.contains(k.hash.as_str())
            || unsure.contains(&k.hash.as_str())
        {
            continue;
        }
        let in_use = active.as_deref() == Some(k.hash.as_str());
        if in_use && !k.disabled {
            continue;
        }
        match (api.delete)(&k.hash) {
            Ok(()) => {
                let why = if in_use {
                    "disabled at OpenRouter"
                } else {
                    "replaced"
                };
                crate::desk::append(log, &event(&k.hash, What::Retired, None, why.into()))?;
                retired += 1;
            }
            Err(e) => {
                fail(&k.hash, format!("deleting it failed: {e}"))?;
                problems.push(format!("{}: {e}", k.hash));
            }
        }
    }
    if retired > 0 {
        did.push(format!("retired {retired}"));
    }
    if !problems.is_empty() {
        return Err(format!(
            "{}; but couldn't delete: {}",
            if did.is_empty() {
                "nothing rotated".into()
            } else {
                did.join(", ")
            },
            problems.join("; ")
        ));
    }
    Ok(if did.is_empty() {
        "nothing due".into()
    } else {
        did.join(", ")
    })
}

/// Records a key pasted in by `scripts/free-key-add.sh`, replacing the provider's last one.
pub fn record_pasted(
    cfg: &Config,
    log: &Path,
    provider: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<String, String> {
    let p = cfg
        .providers
        .get(provider)
        .ok_or_else(|| format!("{provider} isn't in free_keys in the models file"))?;
    if p.auto {
        return Err(format!(
            "{provider} is rotated automatically; don't paste its key"
        ));
    }
    let days = p.expires_days.ok_or("no expires_days")?;
    let _held = lock(log)?;
    let events = read_log(log)?;
    let old = current(&events, provider).map(|e| e.key_id.clone());
    let when = stamp(now);
    let expires = stamp(now + chrono::Duration::days(days));
    let line = |key_id: String, what, expires, note: &str| Event {
        when: when.clone(),
        provider: provider.into(),
        connection: p.connection.clone(),
        key_id,
        what,
        expires,
        note: note.into(),
    };
    // the new key first: if the second line fails, the right key is still the current one
    crate::desk::append(
        log,
        &line(
            format!("pasted-{when}"),
            What::Recorded,
            Some(expires.clone()),
            "pasted with scripts/free-key-add.sh",
        ),
    )?;
    if let Some(old) = old {
        crate::desk::append(
            log,
            &line(old, What::Retired, None, "replaced by a newly pasted key"),
        )?;
    }
    Ok(format!("{provider}: recorded, replace it by {expires}"))
}

/// A pasted provider's OmniRoute connection; refuses one that is rotated automatically.
pub fn pasted_connection(cfg: &Config, provider: &str) -> Result<String, String> {
    let p = cfg
        .providers
        .get(provider)
        .ok_or_else(|| format!("{provider} isn't in free_keys in deploy/models.json"))?;
    if p.auto {
        return Err(format!(
            "{provider} is rotated automatically; run bmv-trader free-keys"
        ));
    }
    Ok(p.connection.clone())
}

/// Whether `omniroute providers list --output json` names `connection` (by name or id).
pub fn listed_has(json: &str, connection: &str) -> Result<bool, String> {
    let v: serde_json::Value = serde_json::from_str(json.trim())
        .map_err(|e| format!("omniroute providers list isn't JSON: {e}"))?;
    let rows = match &v {
        serde_json::Value::Array(r) => r,
        serde_json::Value::Object(o) => o
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or("omniroute providers list: no data array")?,
        _ => return Err("omniroute providers list: not a list".into()),
    };
    Ok(rows
        .iter()
        .any(|r| r["name"].as_str() == Some(connection) || r["id"].as_str() == Some(connection)))
}

/// One health line per provider: its key's days left, and any failure since it went in.
pub fn health(
    cfg: &Config,
    events: &[Event],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<(String, Result<String, String>)> {
    cfg.providers
        .iter()
        .map(|(name, p)| {
            let label = format!("free key: {name}");
            let Some((at, cur)) = current_at(events, name) else {
                return (
                    label,
                    Err(if p.auto {
                        "no key yet: run bmv-trader free-keys".into()
                    } else {
                        format!("no key recorded: run scripts/free-key-add.sh {name}")
                    }),
                );
            };
            let failed_since = events
                .iter()
                .skip(at + 1)
                .any(|e| e.provider == *name && e.what == What::Failed);
            // whole days left, rounding down, so a key that expired an hour ago reads as expired
            let left = cur
                .expires
                .as_deref()
                .and_then(|x| since(x, now))
                .map(|d| (-d.num_hours()).div_euclid(24));
            let overdue = p.auto
                && since(&cur.when, now)
                    .is_some_and(|d| d > chrono::Duration::hours(cfg.rotate_every_hours * 3 / 2));
            let line = match left {
                Some(d) if d < 0 => Err(format!("expired {} days ago", -d)),
                Some(d) if d < 7 && !p.auto => {
                    Err(format!("expires in {d} days: run scripts/free-key-add.sh {name}"))
                }
                Some(d) if failed_since => Err(format!(
                    "a rotation failed since this key went in (data/free_keys.jsonl); it's good for {d} more days"
                )),
                Some(d) if overdue => Err(format!(
                    "rotation is overdue (is the free-keys job running?); key good for {d} more days"
                )),
                Some(d) => Ok(format!("good for {d} more days")),
                None => Ok("in use".into()),
            };
            (label, line)
        })
        .collect()
}

/// The real OpenRouter key API, holding the management key (Keychain `bmv-trader` /
/// `OPENROUTER_MANAGEMENT_KEY`) in a client that is HTTPS-only, with no proxy and no redirect.
pub fn openrouter_api() -> Result<(reqwest::blocking::Client, String), String> {
    let key = crate::config::keychain("OPENROUTER_MANAGEMENT_KEY")?.ok_or(
        "no OPENROUTER_MANAGEMENT_KEY in the Keychain: make one at openrouter.ai/settings/management-keys \
         (with an expiry), then: security add-generic-password -s bmv-trader -a OPENROUTER_MANAGEMENT_KEY -w",
    )?;
    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("OpenRouter client: {e}"))?;
    Ok((http, key))
}

/// The biggest reply read from OpenRouter, so a broken one can't fill memory.
const MAX_REPLY: usize = 1 << 20;

/// One OpenRouter key API request. A reply that isn't JSON is an error, even on success; neither
/// key ever appears in an error.
fn openrouter_call(
    http: &reqwest::blocking::Client,
    mgmt: &str,
    method: reqwest::Method,
    url: &str,
    body: Option<&serde_json::Value>,
) -> Result<(reqwest::StatusCode, serde_json::Value), String> {
    use std::io::Read;
    let mut req = http.request(method, url).bearer_auth(mgmt);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .map_err(|e| format!("OpenRouter: {}", e.without_url()))?;
    let status = resp.status();
    let mut text = String::new();
    resp.take(MAX_REPLY as u64)
        .read_to_string(&mut text)
        .map_err(|e| format!("OpenRouter reply: {e}"))?;
    let v: Result<serde_json::Value, _> = serde_json::from_str(&text);
    if !status.is_success() {
        let why: String = v
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(String::from))
            .unwrap_or_else(|| "refused".into())
            .chars()
            .take(200)
            .collect();
        return Err(format!(
            "OpenRouter {status}: {}",
            why.replace(mgmt, "[key]")
        ));
    }
    v.map(|v| (status, v))
        .map_err(|_| format!("OpenRouter's reply ({status}) isn't JSON"))
}

/// Creates a key with OpenRouter's key API.
pub fn openrouter_create(
    http: &reqwest::blocking::Client,
    mgmt: &str,
    name: &str,
    limit: f64,
    expires: &str,
) -> Result<NewKey, String> {
    let body = serde_json::json!({"name": name, "limit": limit, "expires_at": expires});
    let (_, v) = openrouter_call(
        http,
        mgmt,
        reqwest::Method::POST,
        OPENROUTER_KEYS,
        Some(&body),
    )?;
    let hash = v["data"]["hash"]
        .as_str()
        .ok_or("OpenRouter: no hash for the new key")?;
    let value = v["key"]
        .as_str()
        .ok_or("OpenRouter: no value for the new key")?;
    if !hash.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("OpenRouter: the new key's hash has unexpected characters".into());
    }
    Ok(NewKey {
        hash: hash.into(),
        value: value.into(),
    })
}

/// Deletes a key by hash; one that is already gone (404) counts as deleted.
pub fn openrouter_delete(
    http: &reqwest::blocking::Client,
    mgmt: &str,
    hash: &str,
) -> Result<(), String> {
    if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!(
            "OpenRouter key hash {hash:?} has unexpected characters"
        ));
    }
    match openrouter_call(
        http,
        mgmt,
        reqwest::Method::DELETE,
        &format!("{OPENROUTER_KEYS}/{hash}"),
        None,
    ) {
        Ok((_, v)) if v["deleted"].as_bool() == Some(true) => Ok(()),
        Ok(_) => Err(format!("OpenRouter didn't confirm deleting {hash}")),
        Err(e) if e.starts_with("OpenRouter 404") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Every key on the account, page by page (OpenRouter returns up to 100 at a time).
pub fn openrouter_list(
    http: &reqwest::blocking::Client,
    mgmt: &str,
) -> Result<Vec<Listed>, String> {
    let mut all = Vec::new();
    for page in 0..50 {
        let url = format!("{OPENROUTER_KEYS}?offset={}", page * 100);
        let (_, v) = openrouter_call(http, mgmt, reqwest::Method::GET, &url, None)?;
        let rows = v["data"].as_array().ok_or("OpenRouter: no key list")?;
        all.extend(rows.iter().filter_map(|k| {
            Some(Listed {
                hash: k["hash"].as_str()?.to_string(),
                name: k["name"].as_str().unwrap_or("").to_string(),
                disabled: k["disabled"].as_bool().unwrap_or(false),
            })
        }));
        if rows.len() < 100 {
            return Ok(all);
        }
    }
    Err("OpenRouter: more than 5000 keys; refusing to guess which are ours".into())
}

/// The child environment variable that carries a new key to `omniroute providers rotate`.
pub const NEW_KEY_VAR: &str = KEY_VAR;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn cfg() -> Config {
        Config::parse(
            &serde_json::json!({"free_keys": {"rotate_every_hours": 168, "providers": {
                "openrouter": {"connection": "openrouter-free", "auto": true, "credit_limit_usd": 0},
                "groq": {"connection": "groq-free", "auto": false, "expires_days": 90}}}})
            .to_string(),
        )
        .unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bmv-free-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        log_path(&d)
    }

    fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().into()
    }

    /// A fake OpenRouter: its keys as (hash, name, disabled), and a log of calls.
    struct Fake {
        keys: RefCell<Vec<(String, String, bool)>>,
        calls: RefCell<Vec<String>>,
        next: RefCell<u32>,
    }

    impl Fake {
        fn new(keys: &[(&str, &str, bool)]) -> Fake {
            Fake {
                keys: RefCell::new(
                    keys.iter()
                        .map(|(h, n, d)| (h.to_string(), n.to_string(), *d))
                        .collect(),
                ),
                calls: RefCell::new(vec![]),
                next: RefCell::new(1),
            }
        }
    }

    type Hand<'a> = &'a dyn Fn(&str, &str) -> Result<(), String>;

    fn pass(
        f: &Fake,
        cfg: &Config,
        log: &Path,
        now: &str,
        handover: Hand,
    ) -> Result<String, String> {
        let create = |name: &str, limit: f64, exp: &str| {
            let n = *f.next.borrow();
            *f.next.borrow_mut() += 1;
            let hash = format!("h{n}");
            f.calls
                .borrow_mut()
                .push(format!("create {name} limit={limit} exp={exp}"));
            f.keys.borrow_mut().push((hash.clone(), name.into(), false));
            Ok(NewKey {
                hash,
                value: format!("sk-or-secret-{n}"),
            })
        };
        let delete = |hash: &str| {
            f.calls.borrow_mut().push(format!("delete {hash}"));
            f.keys.borrow_mut().retain(|k| k.0 != hash);
            Ok(())
        };
        let list = || {
            Ok(f.keys
                .borrow()
                .iter()
                .map(|(h, n, d)| Listed {
                    hash: h.clone(),
                    name: n.clone(),
                    disabled: *d,
                })
                .collect())
        };
        let api = KeyApi {
            create: &create,
            delete: &delete,
            list: &list,
        };
        let test = |_: &str| Ok(());
        let omni = OmniRoute {
            handover,
            test: &test,
        };
        run_openrouter(cfg, log, t(now), &api, &omni)
    }

    #[test]
    fn a_rotation_tests_the_new_key_before_deleting_the_old() {
        let (cfg, log) = (cfg(), scratch("rotate"));
        let f = Fake::new(&[("other", "someone-elses-key", false)]);
        let handed = RefCell::new(vec![]);
        let ok = |c: &str, v: &str| {
            handed.borrow_mut().push((c.to_string(), v.to_string()));
            Ok(())
        };
        assert_eq!(
            pass(&f, &cfg, &log, "2026-11-05T12:00:00Z", &ok).unwrap(),
            "rotated"
        );
        assert_eq!(
            handed.borrow()[0],
            ("openrouter-free".into(), "sk-or-secret-1".into())
        );
        assert!(f.calls.borrow()[0].contains("limit=0"), "free models only");
        assert!(
            f.calls.borrow()[0].contains("exp=2026-11-19"),
            "a week + 7 days grace"
        );
        // not due a day later; due a week later, and the old key goes only after the new one works
        assert_eq!(
            pass(&f, &cfg, &log, "2026-11-06T12:00:00Z", &ok).unwrap(),
            "nothing due"
        );
        assert_eq!(
            pass(&f, &cfg, &log, "2026-11-12T12:00:00Z", &ok).unwrap(),
            "rotated, retired 1"
        );
        let calls = f.calls.borrow();
        assert_eq!(calls[calls.len() - 1], "delete h1");
        let keys: Vec<String> = f.keys.borrow().iter().map(|k| k.0.clone()).collect();
        assert_eq!(keys, ["other", "h2"], "someone else's key is never touched");
        let events = read_log(&log).unwrap();
        assert_eq!(current(&events, "openrouter").unwrap().key_id, "h2");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(!text.contains("sk-or-secret"), "no key value in the log");
    }

    #[test]
    fn a_new_key_omniroute_refuses_is_deleted_and_the_old_one_kept() {
        let (cfg, log) = (cfg(), scratch("refused"));
        let f = Fake::new(&[]);
        let ok = |_: &str, _: &str| Ok(());
        pass(&f, &cfg, &log, "2026-11-05T12:00:00Z", &ok).unwrap();
        let bad = |_: &str, v: &str| Err(format!("test failed for {v}"));
        let e = pass(&f, &cfg, &log, "2026-11-13T12:00:00Z", &bad).unwrap_err();
        assert!(
            e.contains("still works on its old key") && !e.contains("sk-or-secret"),
            "{e}"
        );
        let keys: Vec<String> = f.keys.borrow().iter().map(|k| k.0.clone()).collect();
        assert_eq!(keys, ["h1"], "the new key is gone and the old one stays");
        assert_eq!(
            current(&read_log(&log).unwrap(), "openrouter")
                .unwrap()
                .key_id,
            "h1"
        );
    }

    #[test]
    fn a_disabled_key_in_use_is_retired() {
        let (cfg, log) = (cfg(), scratch("disabled"));
        let f = Fake::new(&[]);
        let ok = |_: &str, _: &str| Ok(());
        pass(&f, &cfg, &log, "2026-11-05T12:00:00Z", &ok).unwrap();
        f.keys.borrow_mut()[0].2 = true;
        assert_eq!(
            pass(&f, &cfg, &log, "2026-11-06T12:00:00Z", &ok).unwrap(),
            "retired 1"
        );
        assert!(current(&read_log(&log).unwrap(), "openrouter").is_none());
        // the next pass sees no key in use and makes one
        assert_eq!(
            pass(&f, &cfg, &log, "2026-11-06T13:00:00Z", &ok).unwrap(),
            "rotated"
        );
    }

    #[test]
    fn pasted_keys_are_tracked_for_expiry() {
        let (cfg, log) = (cfg(), scratch("pasted"));
        record_pasted(&cfg, &log, "groq", t("2026-11-05T12:00:00Z")).unwrap();
        let ev = read_log(&log).unwrap();
        let h = health(&cfg, &ev, t("2026-11-06T12:00:00Z"));
        let groq = h.iter().find(|(l, _)| l == "free key: groq").unwrap();
        assert!(
            groq.1.as_ref().unwrap().contains("89 more days"),
            "{:?}",
            groq.1
        );
        let late = health(&cfg, &ev, t("2027-01-30T12:00:00Z"));
        assert!(
            late.iter()
                .find(|(l, _)| l == "free key: groq")
                .unwrap()
                .1
                .is_err()
        );
        assert!(record_pasted(&cfg, &log, "openrouter", t("2026-11-05T12:00:00Z")).is_err());
    }

    #[test]
    fn pasted_connections_and_the_omniroute_list() {
        let c = cfg();
        assert_eq!(pasted_connection(&c, "groq").unwrap(), "groq-free");
        assert!(
            pasted_connection(&c, "openrouter").is_err(),
            "rotated automatically"
        );
        assert!(pasted_connection(&c, "nobody").is_err());
        assert!(listed_has(r#"[{"id": "1", "name": "groq-free"}]"#, "groq-free").unwrap());
        assert!(listed_has(r#"{"data": [{"id": "groq-free"}]}"#, "groq-free").unwrap());
        assert!(!listed_has(r#"[{"name": "other"}]"#, "groq-free").unwrap());
        assert!(listed_has("not json", "groq-free").is_err());
    }

    #[test]
    fn only_keys_this_job_made_are_ever_deleted() {
        let (cfg, log) = (cfg(), scratch("ours"));
        // someone else's key with our prefix, made by hand
        let f = Fake::new(&[("manual", "bmv-free-by-hand", false)]);
        let ok = |_: &str, _: &str| Ok(());
        pass(&f, &cfg, &log, "2026-11-05T12:00:00Z", &ok).unwrap();
        pass(&f, &cfg, &log, "2026-11-12T12:00:00Z", &ok).unwrap();
        let keys: Vec<String> = f.keys.borrow().iter().map(|k| k.0.clone()).collect();
        assert_eq!(keys, ["manual", "h2"], "the hand-made key survives");
    }

    #[test]
    fn a_pending_key_is_never_deleted_and_forces_a_new_rotation() {
        let (cfg, log) = (cfg(), scratch("pending"));
        let f = Fake::new(&[]);
        let ok = |_: &str, _: &str| Ok(());
        pass(&f, &cfg, &log, "2026-11-05T12:00:00Z", &ok).unwrap();
        // a crash between handover and logging Active: h2 is Created, nothing after
        f.keys
            .borrow_mut()
            .push(("h2".into(), "bmv-free-20261106".into(), false));
        *f.next.borrow_mut() = 3;
        crate::desk::append(
            &log,
            &Event {
                when: "2026-11-06T12:00:00Z".into(),
                provider: "openrouter".into(),
                connection: "openrouter-free".into(),
                key_id: "h2".into(),
                what: What::Created,
                expires: None,
                note: "".into(),
            },
        )
        .unwrap();
        // not due by time, but the pending key forces a rotation, and h2 is left alone
        assert_eq!(
            pass(&f, &cfg, &log, "2026-11-06T13:00:00Z", &ok).unwrap(),
            "rotated, retired 1"
        );
        let keys: Vec<String> = f.keys.borrow().iter().map(|k| k.0.clone()).collect();
        assert_eq!(keys, ["h2", "h3"], "h1 retired, pending h2 kept, h3 in use");
    }

    #[test]
    fn failures_are_logged_and_flagged_by_health() {
        let (cfg, log) = (cfg(), scratch("health"));
        let f = Fake::new(&[]);
        let ok = |_: &str, _: &str| Ok(());
        pass(&f, &cfg, &log, "2026-11-05T12:00:00Z", &ok).unwrap();
        let bad = |_: &str, _: &str| Err("refused".to_string());
        assert!(pass(&f, &cfg, &log, "2026-11-12T12:00:00Z", &bad).is_err());
        let ev = read_log(&log).unwrap();
        let or = |at: &str| {
            health(&cfg, &ev, t(at))
                .into_iter()
                .find(|(l, _)| l == "free key: openrouter")
                .unwrap()
        };
        let line = or("2026-11-12T13:00:00Z");
        assert!(
            line.1.as_ref().unwrap_err().contains("rotation failed"),
            "{:?}",
            line.1
        );
        // a key that expired an hour ago reads as expired, not "0 more days"
        let late = or("2026-11-19T13:00:00Z");
        assert!(
            late.1.as_ref().unwrap_err().contains("expired"),
            "{:?}",
            late.1
        );
    }

    #[test]
    fn only_one_pass_at_a_time() {
        let log = scratch("lock");
        let held = lock(&log).unwrap();
        assert!(lock(&log).is_err());
        drop(held);
        assert!(lock(&log).is_ok());
    }

    #[test]
    fn the_config_is_checked() {
        let with = |p: serde_json::Value| {
            Config::parse(
                &serde_json::json!({"free_keys": {"rotate_every_hours": 168,
                "providers": {"groq": p}}})
                .to_string(),
            )
        };
        assert!(
            with(serde_json::json!({"connection": "groq-free", "auto": true})).is_err(),
            "no key API"
        );
        assert!(
            with(serde_json::json!({"connection": "groq-free", "auto": false})).is_err(),
            "needs expires_days"
        );
        assert!(
            with(serde_json::json!({"connection": "groq free", "auto": false, "expires_days": 90}))
                .is_err()
        );
        assert!(
            with(serde_json::json!({"connection": "groq-free", "auto": false, "expires_days": 90}))
                .is_ok()
        );
        let fast = Config::parse(
            &serde_json::json!({"free_keys": {"rotate_every_hours": 1, "providers": {}}})
                .to_string(),
        );
        assert!(fast.is_err(), "no rotating every few minutes");
    }
}
