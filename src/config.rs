//! Paper by default. Live needs TRADING_MODE=live, the exact LIVE_CONFIRM phrase (surrounding
//! whitespace ignored), and separate live keys. Settings come from `$BMV_HOME/.env` only, never the
//! shell. Keys come from the macOS Keychain (service `bmv-trader`, account = the variable name);
//! paper keys may instead sit in `.env`, live keys never may.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const LIVE_CONFIRM_PHRASE: &str = "yes-i-accept-real-losses";

/// Base directory for `.env` and `data/`. Never the current directory: under launchd that is `/`.
pub fn home() -> Result<PathBuf, String> {
    let Some(h) = std::env::var_os("BMV_HOME") else {
        // shortcut: defaults to the source checkout, set BMV_HOME when deploying a release binary (D2).
        return Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    };
    let p = PathBuf::from(h);
    if p.is_absolute() && p.is_dir() {
        Ok(p)
    } else {
        Err(format!(
            "BMV_HOME must be an existing absolute directory, got {p:?}"
        ))
    }
}

/// Loads `$BMV_HOME/.env` and builds the Config. Refuses trading settings found in the shell,
/// because they would otherwise silently disagree with `.env`.
pub fn load() -> Result<Config, String> {
    if let Some(k) = shell_conflict(std::env::vars_os().filter_map(|(k, _)| k.into_string().ok())) {
        return Err(format!(
            "{k} is set in your shell; unset it (config comes from .env only)"
        ));
    }
    from_map(&read_env_file(&home()?.join(".env"))?, keychain)
}

pub const KEYCHAIN_SERVICE: &str = "bmv-trader";

/// One Keychain item, via Apple's own CLI (absolute path, so PATH can't swap it). Not found is
/// None; any other failure, such as a locked keychain, a denied prompt or a hang, is an error.
pub(crate) fn keychain(account: &str) -> Result<Option<String>, String> {
    keychain_in(KEYCHAIN_SERVICE, account)
}

/// Whether a Keychain item exists, without reading its value. Only "not found" (exit 44) is false;
/// a locked keychain, a denied prompt or a hang is an error, never "missing".
pub fn keychain_present(service: &str, account: &str) -> Result<bool, String> {
    use std::time::{Duration, Instant};
    const TIMEOUT: Duration = Duration::from_secs(10);
    let mut child = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run /usr/bin/security: {e}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Keychain check for {account} timed out; is the keychain unlocked?"
                ));
            }
            Err(e) => return Err(format!("Keychain check for {account} failed: {e}")),
        }
    };
    present_reply(account, status.code())
}

fn present_reply(account: &str, code: Option<i32>) -> Result<bool, String> {
    match code {
        Some(0) => Ok(true),
        Some(44) => Ok(false),
        Some(c) => Err(format!(
            "Keychain check for {account} failed (security exit {c})"
        )),
        None => Err(format!(
            "Keychain check for {account} was stopped by a signal"
        )),
    }
}

/// The same, for an item under another service (the desk agents' keys use `bmv-desk-models`).
pub fn keychain_in(service: &str, account: &str) -> Result<Option<String>, String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    // A locked keychain over SSH can wait forever on a prompt; startup must fail instead of hang.
    const TIMEOUT: Duration = Duration::from_secs(10);
    let mut child = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run /usr/bin/security for {account}: {e}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Keychain lookup for {account} timed out; is the keychain unlocked?"
                ));
            }
            Err(e) => return Err(format!("Keychain lookup for {account} failed: {e}")),
        }
    };
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        out.read_to_end(&mut stdout)
            .map_err(|e| format!("cannot read Keychain reply for {account}: {e}"))?;
    }
    keychain_reply(account, status.code(), &stdout)
}

/// `security` exits 44 when the item doesn't exist. Its stderr is never echoed: keep errors free of secrets.
fn keychain_reply(
    account: &str,
    code: Option<i32>,
    stdout: &[u8],
) -> Result<Option<String>, String> {
    match code {
        Some(0) => {
            let v = std::str::from_utf8(stdout)
                .map_err(|_| format!("Keychain item {account} is not valid UTF-8"))?
                .trim()
                .to_string();
            // An empty item would let a paper key fall through to .env; refuse it instead.
            if v.is_empty() {
                return Err(format!(
                    "Keychain item {account} is empty; delete it or set a value"
                ));
            }
            Ok(Some(v))
        }
        Some(44) => Ok(None),
        other => Err(format!(
            "Keychain lookup for {account} failed (security exit {other:?}); is the keychain unlocked?"
        )),
    }
}

fn shell_conflict(mut names: impl Iterator<Item = String>) -> Option<String> {
    names.find(|k| k == "TRADING_MODE" || k == "LIVE_CONFIRM" || k.starts_with("ALPACA_"))
}

/// A missing file is an empty map (every key then reports "not set"); a readable-by-others file is refused.
fn read_env_file(path: &Path) -> Result<HashMap<String, String>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is readable by other users; run: chmod 600 {0}",
            path.display()
        ));
    }
    dotenvy::from_path_iter(path)
        .and_then(|it| it.collect())
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// Values are trimmed; empty counts as missing.
fn from_map(
    vars: &HashMap<String, String>,
    keychain: impl Fn(&str) -> Result<Option<String>, String>,
) -> Result<Config, String> {
    from_sources(
        |k| {
            vars.get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        },
        keychain,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Paper,
    Live,
}

impl Mode {
    pub fn base_url(self) -> &'static str {
        match self {
            Mode::Paper => "https://paper-api.alpaca.markets",
            Mode::Live => "https://api.alpaca.markets",
        }
    }
}

/// Prints as `***` so `{:?}` of a Config can never leak the key.
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// Fields are private: `from_env` is the only way to build one, so mode and keys can't mismatch.
#[derive(Debug)]
pub struct Config {
    mode: Mode,
    key_id: String,
    secret: Secret,
}

impl Config {
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn key_id(&self) -> &str {
        &self.key_id
    }
    pub fn secret(&self) -> &str {
        &self.secret.0
    }
}

/// `.env` only, no Keychain: paper works, live is always refused (its keys must be in the Keychain).
pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    from_sources(get, |_| Ok(None))
}

/// `env` returns a `.env` value (empty = missing); `keychain` looks up one Keychain item.
pub fn from_sources(
    env: impl Fn(&str) -> Option<String>,
    keychain: impl Fn(&str) -> Result<Option<String>, String>,
) -> Result<Config, String> {
    let get = &env;
    // A key in both places could silently disagree, so that's refused like a shell override.
    let key = |k: &str, env_allowed: bool| -> Result<String, String> {
        match (keychain(k)?, env(k)) {
            (Some(_), Some(_)) => Err(format!(
                "{k} is in both the Keychain and .env; remove it from .env"
            )),
            (Some(v), None) => Ok(v),
            (None, Some(_)) if !env_allowed => Err(format!(
                "{k} must not be in .env; store it in the Keychain: \
                 security add-generic-password -s {KEYCHAIN_SERVICE} -a {k} -w"
            )),
            (None, Some(v)) => Ok(v),
            (None, None) => Err(format!(
                "{k} is not set (Keychain service {KEYCHAIN_SERVICE}{})",
                if env_allowed { ", or .env" } else { "" }
            )),
        }
    };
    match get("TRADING_MODE").as_deref().unwrap_or("paper") {
        "paper" => Ok(Config {
            mode: Mode::Paper,
            key_id: key("ALPACA_PAPER_KEY_ID", true)?,
            secret: Secret(key("ALPACA_PAPER_SECRET", true)?),
        }),
        "live" => {
            if get("LIVE_CONFIRM").as_deref() != Some(LIVE_CONFIRM_PHRASE) {
                return Err(format!(
                    "TRADING_MODE=live refused: set LIVE_CONFIRM={LIVE_CONFIRM_PHRASE}"
                ));
            }
            Ok(Config {
                mode: Mode::Live,
                key_id: key("ALPACA_LIVE_KEY_ID", false)?,
                secret: Secret(key("ALPACA_LIVE_SECRET", false)?),
            })
        }
        other => Err(format!(
            "TRADING_MODE must be 'paper' or 'live', got '{other}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_not_found_means_a_key_is_missing() {
        assert_eq!(present_reply("k", Some(0)), Ok(true));
        assert_eq!(present_reply("k", Some(44)), Ok(false));
        // 36 and 51 are what a locked keychain or a denied prompt give: unknown, never missing
        assert!(present_reply("k", Some(36)).is_err());
        assert!(present_reply("k", None).is_err());
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone())
    }

    fn no_keychain(_: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn keychain_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<Option<String>, String> {
        let get = env(pairs);
        move |k| Ok(get(k))
    }

    const LIVE_ON: [(&str, &str); 2] = [
        ("TRADING_MODE", "live"),
        ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
    ];

    const PAPER: [(&str, &str); 2] = [("ALPACA_PAPER_KEY_ID", "pk"), ("ALPACA_PAPER_SECRET", "ps")];
    const LIVE: [(&str, &str); 2] = [("ALPACA_LIVE_KEY_ID", "lk"), ("ALPACA_LIVE_SECRET", "ls")];

    #[test]
    fn defaults_to_paper() {
        let c = from_env(env(&PAPER)).unwrap();
        assert_eq!(c.mode(), Mode::Paper);
        assert_eq!(c.mode().base_url(), "https://paper-api.alpaca.markets");
        assert_eq!(c.key_id(), "pk");
    }

    #[test]
    fn debug_never_prints_secret() {
        let c = from_env(env(&[
            PAPER[0],
            ("ALPACA_PAPER_SECRET", "s3cret-VALUE-xyz"),
        ]))
        .unwrap();
        let shown = format!("{c:?}");
        assert!(
            shown.contains("***") && !shown.contains("s3cret-VALUE-xyz"),
            "{shown}"
        );
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn values_are_trimmed_and_blank_means_missing() {
        let m = map(&[
            ("TRADING_MODE", "  live "),
            LIVE[0],
            LIVE[1],
            ("LIVE_CONFIRM", " "),
        ]);
        assert!(from_map(&m, no_keychain).unwrap_err().contains("refused"));
        let m = map(&[
            ("TRADING_MODE", ""),
            PAPER[0],
            ("ALPACA_PAPER_SECRET", "  ps  "),
        ]);
        let c = from_map(&m, no_keychain).unwrap();
        assert_eq!((c.mode(), c.secret()), (Mode::Paper, "ps"));
        assert!(
            from_map(
                &map(&[PAPER[0], ("ALPACA_PAPER_SECRET", "   ")]),
                no_keychain
            )
            .is_err()
        );
    }

    #[test]
    fn trading_settings_in_shell_are_detected() {
        let names = |v: &[&str]| {
            v.iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert_eq!(shell_conflict(names(&["PATH", "HOME"])), None);
        assert_eq!(shell_conflict(names(&["ANTHROPIC_API_KEY"])), None);
        for k in [
            "TRADING_MODE",
            "LIVE_CONFIRM",
            "ALPACA_LIVE_SECRET",
            "ALPACA_PAPER_KEY_ID",
        ] {
            assert_eq!(shell_conflict(names(&["PATH", k])).as_deref(), Some(k));
        }
    }

    fn temp_env(contents: &str, mode: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bmv-test-{}-{mode:o}-{}",
            std::process::id(),
            contents.len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".env");
        std::fs::write(&p, contents).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn env_file_must_be_private() {
        let open = temp_env("ALPACA_PAPER_KEY_ID=pk\n", 0o644);
        assert!(read_env_file(&open).unwrap_err().contains("chmod 600"));
        let private = temp_env("ALPACA_PAPER_KEY_ID=pk\nALPACA_PAPER_SECRET=ps\n", 0o600);
        assert_eq!(
            read_env_file(&private).unwrap()["ALPACA_PAPER_SECRET"],
            "ps"
        );
    }

    #[test]
    fn env_file_errors_are_reported() {
        let broken = temp_env("BAD LINE \"unterminated\n", 0o600);
        assert!(read_env_file(&broken).unwrap_err().contains("cannot parse"));
        assert!(
            read_env_file(Path::new("/nonexistent/bmv/.env"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paper_needs_paper_keys() {
        assert!(from_env(env(&LIVE)).is_err());
    }

    #[test]
    fn live_refused_without_confirm() {
        let e = from_env(env(&[("TRADING_MODE", "live"), LIVE[0], LIVE[1]])).unwrap_err();
        assert!(e.contains("refused"));
    }

    #[test]
    fn live_refused_with_wrong_confirm() {
        let vars = [
            ("TRADING_MODE", "live"),
            ("LIVE_CONFIRM", "yes"),
            LIVE[0],
            LIVE[1],
        ];
        assert!(from_env(env(&vars)).is_err());
    }

    #[test]
    fn live_never_uses_paper_keys() {
        let vars = [
            ("TRADING_MODE", "live"),
            ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
            PAPER[0],
            PAPER[1],
        ];
        assert!(from_env(env(&vars)).is_err());
    }

    #[test]
    fn live_with_everything() {
        let c = from_sources(env(&LIVE_ON), keychain_of(&LIVE)).unwrap();
        assert_eq!(c.mode(), Mode::Live);
        assert_eq!(c.mode().base_url(), "https://api.alpaca.markets");
        assert_eq!((c.key_id(), c.secret()), ("lk", "ls"));
    }

    #[test]
    fn live_keys_in_env_are_refused_even_when_complete() {
        let vars = [LIVE_ON[0], LIVE_ON[1], LIVE[0], LIVE[1]];
        let e = from_sources(env(&vars), no_keychain).unwrap_err();
        assert!(
            e.contains("must not be in .env") && e.contains("add-generic-password"),
            "{e}"
        );
        // half in the Keychain, half in .env is still refused
        let e = from_sources(
            env(&[LIVE_ON[0], LIVE_ON[1], LIVE[1]]),
            keychain_of(&[LIVE[0]]),
        )
        .unwrap_err();
        assert!(e.contains("ALPACA_LIVE_SECRET must not be in .env"), "{e}");
        assert!(
            from_sources(env(&LIVE_ON), no_keychain)
                .unwrap_err()
                .contains("not set")
        );
    }

    #[test]
    fn paper_keys_come_from_the_keychain_or_env() {
        let c = from_sources(env(&[]), keychain_of(&PAPER)).unwrap();
        assert_eq!(
            (c.mode(), c.key_id(), c.secret()),
            (Mode::Paper, "pk", "ps")
        );
        let c = from_sources(env(&PAPER), no_keychain).unwrap();
        assert_eq!(c.secret(), "ps");
        let c = from_sources(env(&[PAPER[0]]), keychain_of(&[PAPER[1]])).unwrap();
        assert_eq!((c.key_id(), c.secret()), ("pk", "ps"));
    }

    #[test]
    fn a_key_in_both_places_is_refused() {
        let e = from_sources(env(&PAPER), keychain_of(&PAPER)).unwrap_err();
        assert!(e.contains("both the Keychain and .env"), "{e}");
    }

    #[test]
    fn keychain_errors_stop_startup_instead_of_falling_back_to_env() {
        let locked = |_: &str| Err("Keychain lookup failed".to_string());
        assert!(from_sources(env(&PAPER), locked).is_err());
    }

    #[test]
    fn keychain_replies_are_read_strictly() {
        assert_eq!(
            keychain_reply("K", Some(0), b"abc\n").unwrap().as_deref(),
            Some("abc")
        );
        assert!(
            keychain_reply("K", Some(0), b"  \n")
                .unwrap_err()
                .contains("empty")
        );
        assert_eq!(keychain_reply("K", Some(44), b"").unwrap(), None);
        for code in [Some(1), Some(51), Some(128), None] {
            let e = keychain_reply("K", code, b"").unwrap_err();
            assert!(e.contains("unlocked"), "{code:?}: {e}");
        }
        assert!(keychain_reply("K", Some(0), &[0xff, 0xfe]).is_err());
    }

    #[test]
    fn the_real_keychain_never_invents_a_missing_item() {
        // The real CLI on an account that can't exist: None here, or an error on a machine without
        // a usable keychain (CI, sandbox), but never a value.
        if std::path::Path::new("/usr/bin/security").exists() {
            let got = keychain("bmv-trader-test-nonexistent-account");
            assert!(!matches!(got, Ok(Some(_))), "{got:?}");
        }
    }

    #[test]
    fn live_confirm_is_checked_before_any_keychain_lookup() {
        let asked = std::cell::Cell::new(false);
        let spy = |_: &str| {
            asked.set(true);
            Ok(None)
        };
        assert!(from_sources(env(&[("TRADING_MODE", "live")]), spy).is_err());
        assert!(!asked.get());
    }

    #[test]
    fn unknown_mode_rejected() {
        assert!(from_env(env(&[("TRADING_MODE", "Live"), PAPER[0], PAPER[1]])).is_err());
    }
}
