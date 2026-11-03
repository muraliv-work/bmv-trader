// shortcut: blocking HTTP, switch to async reqwest + tokio when the market-data websocket lands (P3).
use crate::config::Config;
use crate::money::Usd;
use reqwest::header::HeaderValue;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use std::time::Duration;

/// Required fields only: a missing or renamed field is an error, never a silent `null`.
#[derive(Debug, Deserialize)]
pub struct Account {
    pub status: String,
    pub currency: String,
    #[serde(deserialize_with = "usd")]
    pub cash: Usd,
    #[serde(deserialize_with = "usd")]
    pub buying_power: Usd,
    #[serde(deserialize_with = "usd")]
    pub equity: Usd,
    /// Equity at the previous close; `equity − last_equity` is today's P&L for the daily halt.
    #[serde(deserialize_with = "usd")]
    pub last_equity: Usd,
    pub trading_blocked: bool,
    pub account_blocked: bool,
}

impl Account {
    /// Every limit assumes an active, unblocked USD account with a known baseline; anything else means stop.
    pub fn validate(&self) -> Result<(), String> {
        let problem = if self.currency != "USD" {
            format!("currency is {:?}, expected \"USD\"", self.currency)
        } else if self.status != "ACTIVE" {
            format!("status is {:?}, expected \"ACTIVE\"", self.status)
        } else if self.trading_blocked || self.account_blocked {
            "account is blocked".to_string()
        } else if self.equity <= Usd::ZERO || self.last_equity <= Usd::ZERO {
            format!(
                "equity {} / last_equity {} must both be > 0",
                self.equity, self.last_equity
            )
        } else {
            return Ok(());
        };
        Err(format!("Alpaca account unusable: {problem}"))
    }
}

/// Error bodies end up in logs and the journal, so they're capped.
fn clip(s: &str) -> &str {
    s.char_indices().nth(512).map_or(s, |(i, _)| &s[..i])
}

/// Alpaca sends money as decimal strings.
fn usd<'de, D: Deserializer<'de>>(d: D) -> Result<Usd, D::Error> {
    Usd::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
}

pub struct Alpaca {
    http: reqwest::blocking::Client,
    cfg: Config,
}

/// Which Alpaca host a read goes to. Market data is the same host for paper and live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Trading,
    MarketData,
}

impl Alpaca {
    pub fn new(cfg: Config) -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            // reqwest keeps custom headers on cross-host redirects, which would hand the keys to that host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("cannot build HTTP client: {e}"))?;
        Ok(Self { http, cfg })
    }

    /// The account, already validated (see `Account::validate`).
    pub fn account(&self) -> Result<Account, String> {
        let a: Account = self.get(Api::Trading, "/v2/account")?;
        a.validate()?;
        Ok(a)
    }

    /// A read-only GET. There is deliberately no generic POST: orders get their own typed method.
    pub fn get<T: DeserializeOwned>(&self, api: Api, path: &str) -> Result<T, String> {
        let base = match api {
            Api::Trading => self.cfg.mode().base_url(),
            Api::MarketData => "https://data.alpaca.markets",
        };
        let mut secret = HeaderValue::from_str(self.cfg.secret())
            .map_err(|_| "Alpaca secret contains characters not allowed in a header".to_string())?;
        secret.set_sensitive(true);
        let resp = self
            .http
            .get(format!("{base}{path}"))
            .header("APCA-API-KEY-ID", self.cfg.key_id())
            .header("APCA-API-SECRET-KEY", secret)
            .send()
            .map_err(|e| format!("request to Alpaca {path} failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .map_err(|e| format!("reading Alpaca {path} response (HTTP {status}): {e}"))?;
        if !status.is_success() {
            return Err(format!("Alpaca {path} HTTP {status}: {}", clip(&body)));
        }
        serde_json::from_str(&body)
            .map_err(|e| format!("unexpected response from Alpaca {path}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = r#"{"status":"ACTIVE","currency":"USD","cash":"25000.00",
        "buying_power":"100000.00","equity":"25000.00","last_equity":"24950.10",
        "trading_blocked":false,"account_blocked":false,"extra_field":1}"#;

    #[test]
    fn parses_account() {
        let a: Account = serde_json::from_str(ACCOUNT).unwrap();
        assert_eq!(a.equity, Usd::from_cents(2_500_000));
        assert_eq!((a.equity - a.last_equity).cents(), 4_990);
        assert!(!a.trading_blocked);
    }

    #[test]
    fn validate_accepts_a_normal_account() {
        let a: Account = serde_json::from_str(ACCOUNT).unwrap();
        assert!(a.validate().is_ok());
    }

    #[test]
    fn validate_rejects_anything_unexpected() {
        for (from, to) in [
            (r#""currency":"USD""#, r#""currency":"CAD""#),
            (r#""status":"ACTIVE""#, r#""status":"ACCOUNT_CLOSED""#),
            (r#""trading_blocked":false"#, r#""trading_blocked":true"#),
            (r#""account_blocked":false"#, r#""account_blocked":true"#),
            (r#""equity":"25000.00""#, r#""equity":"0""#),
            (r#""last_equity":"24950.10""#, r#""last_equity":"0""#),
            (r#""equity":"25000.00""#, r#""equity":"-5.00""#),
        ] {
            let a: Account = serde_json::from_str(&ACCOUNT.replace(from, to)).unwrap();
            assert!(a.validate().is_err(), "should reject {to}");
        }
    }

    #[test]
    fn error_bodies_are_capped_on_char_boundaries() {
        assert_eq!(clip("short"), "short");
        let long = "é".repeat(600);
        assert_eq!(clip(&long).chars().count(), 512);
    }

    #[test]
    fn non_numeric_money_is_an_error() {
        let bad = ACCOUNT.replace(r#""cash":"25000.00""#, r#""cash":"n/a""#);
        assert!(serde_json::from_str::<Account>(&bad).is_err());
    }

    #[test]
    fn missing_field_is_an_error() {
        let without_equity = ACCOUNT.replace(r#""equity":"25000.00","#, "");
        assert!(serde_json::from_str::<Account>(&without_equity).is_err());
    }
}
