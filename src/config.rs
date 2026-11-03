//! Paper by default. Live needs TRADING_MODE=live, the exact LIVE_CONFIRM phrase (surrounding
//! whitespace ignored), and separate live keys. Config comes from `$BMV_HOME/.env` only, never the shell.

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
    from_map(&read_env_file(&home()?.join(".env"))?)
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
fn from_map(vars: &HashMap<String, String>) -> Result<Config, String> {
    from_env(|k| {
        vars.get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
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

/// `get` returns a variable's value, with empty treated as missing.
pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    let need = |k: &str| get(k).ok_or_else(|| format!("{k} is not set in .env"));
    match get("TRADING_MODE").as_deref().unwrap_or("paper") {
        "paper" => Ok(Config {
            mode: Mode::Paper,
            key_id: need("ALPACA_PAPER_KEY_ID")?,
            secret: Secret(need("ALPACA_PAPER_SECRET")?),
        }),
        "live" => {
            if get("LIVE_CONFIRM").as_deref() != Some(LIVE_CONFIRM_PHRASE) {
                return Err(format!(
                    "TRADING_MODE=live refused: set LIVE_CONFIRM={LIVE_CONFIRM_PHRASE}"
                ));
            }
            Ok(Config {
                mode: Mode::Live,
                key_id: need("ALPACA_LIVE_KEY_ID")?,
                secret: Secret(need("ALPACA_LIVE_SECRET")?),
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

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone())
    }

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
        assert!(from_map(&m).unwrap_err().contains("refused"));
        let m = map(&[
            ("TRADING_MODE", ""),
            PAPER[0],
            ("ALPACA_PAPER_SECRET", "  ps  "),
        ]);
        let c = from_map(&m).unwrap();
        assert_eq!((c.mode(), c.secret()), (Mode::Paper, "ps"));
        assert!(from_map(&map(&[PAPER[0], ("ALPACA_PAPER_SECRET", "   ")])).is_err());
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
        let vars = [
            ("TRADING_MODE", "live"),
            ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
            LIVE[0],
            LIVE[1],
        ];
        let c = from_env(env(&vars)).unwrap();
        assert_eq!(c.mode(), Mode::Live);
        assert_eq!(c.mode().base_url(), "https://api.alpaca.markets");
        assert_eq!(c.secret(), "ls");
    }

    #[test]
    fn unknown_mode_rejected() {
        assert!(from_env(env(&[("TRADING_MODE", "Live"), PAPER[0], PAPER[1]])).is_err());
    }
}
