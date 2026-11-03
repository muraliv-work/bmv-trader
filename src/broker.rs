// shortcut: blocking HTTP, switch to async reqwest + tokio when the market-data websocket lands (P3).
use crate::config::Config;
use crate::money::{Price, Qty, Usd};
use crate::risk::OrderIntent;
use reqwest::Method;
use reqwest::header::HeaderValue;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
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

/// One Alpaca order, only the fields reconcile needs. Bracket and OCO legs come nested in `legs`.
#[derive(Debug, Deserialize)]
pub struct Order {
    pub id: String,
    pub client_order_id: String,
    pub symbol: String,
    pub side: String,
    pub status: String,
    #[serde(deserialize_with = "shares")]
    pub qty: u32,
    #[serde(deserialize_with = "shares")]
    pub filled_qty: u32,
    #[serde(deserialize_with = "opt_price")]
    pub filled_avg_price: Option<Price>,
    #[serde(default)]
    pub legs: Option<Vec<Order>>,
}

#[derive(Debug, Deserialize)]
pub struct Position {
    pub symbol: String,
    /// Negative for a short, which this system never opens on purpose.
    #[serde(deserialize_with = "signed_shares")]
    pub qty: i64,
    #[serde(deserialize_with = "price")]
    pub avg_entry_price: Price,
}

/// After a submit error, did the order reach Alpaca?
#[derive(Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// Refused here before any request (bad id, mode mismatch, bad qty, ...): a bug, nothing sent.
    Invalid(String),
    /// Alpaca answered and refused it; nothing was placed.
    Rejected(String),
    /// It may or may not exist. Reconcile by client_order_id before anything else (DESIGN §8).
    Unknown(String),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let (SubmitError::Invalid(m) | SubmitError::Rejected(m) | SubmitError::Unknown(m)) = self;
        f.write_str(m)
    }
}

/// Alpaca error codes we act on. [verify codes against recorded paper replies]
const INSUFFICIENT_BUYING_POWER: u64 = 40310000;
const UNPROCESSABLE: u64 = 42210000;
const NOT_FOUND: u64 = 40410000;

/// Alpaca's error JSON `{"code": n, "message": ".."}`; None if the body isn't that shape.
fn error_code(body: &str) -> Option<(u64, String)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let msg = v["message"].as_str().unwrap_or_default().to_string();
    Some((v["code"].as_u64()?, msg))
}

/// Only a known definite "no" (buying power, invalid price/qty) counts as Rejected. A
/// client_order_id complaint means an order with that id may already exist, so it's Unknown,
/// as is anything we don't recognise.
fn classify(status: u16, body: &str) -> SubmitError {
    let msg = format!("Alpaca order HTTP {status}: {}", clip(body));
    let refused = match error_code(body) {
        Some((INSUFFICIENT_BUYING_POWER, _)) => status == 403,
        Some((UNPROCESSABLE, m)) => status == 422 && !m.contains("client_order_id"),
        _ => false,
    };
    if refused {
        SubmitError::Rejected(msg)
    } else {
        SubmitError::Unknown(msg)
    }
}

/// Only Alpaca's own "not found" JSON means absent; a 404 from a proxy or a moved route doesn't.
fn is_not_found(status: u16, body: &str) -> bool {
    status == 404 && matches!(error_code(body), Some((NOT_FOUND, _)))
}

/// Our OCO ids are the entry id plus this, so a resend of the same OCO is a duplicate Alpaca refuses.
const OCO_SUFFIX: &str = "-x";

/// US equity tickers: uppercase letters, and '.' for share classes (BRK.B).
fn valid_symbol(s: &str) -> bool {
    (1..=10).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase() || b == b'.')
}

/// `bmv-{YYYY-MM-DD}-{SYMBOL}-{seq}`, the only entry id shape we make; anything else is a bug.
fn valid_entry_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("bmv-") else {
        return false;
    };
    let (Some(date), Some(rest)) = (rest.get(..10), rest.get(10..)) else {
        return false;
    };
    let date_ok = date.bytes().enumerate().all(|(i, b)| match i {
        4 | 7 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    let Some((sym, seq)) = rest.strip_prefix('-').and_then(|r| r.rsplit_once('-')) else {
        return false;
    };
    date_ok
        && valid_symbol(sym)
        && (1..=9).contains(&seq.len())
        && seq.bytes().all(|b| b.is_ascii_digit())
}

/// An entry id, or an entry id plus the OCO suffix.
fn valid_client_id(id: &str) -> bool {
    valid_entry_id(id.strip_suffix(OCO_SUFFIX).unwrap_or(id))
}

/// An approved intent bound to the one client id it may ever be sent under. Not Clone.
#[derive(Debug)]
pub struct Bracket {
    id: String,
    intent: OrderIntent,
}

impl Bracket {
    pub fn new(id: String, intent: OrderIntent) -> Self {
        Bracket { id, intent }
    }
    pub fn client_id(&self) -> &str {
        &self.id
    }
    /// For an Unknown submit that reconcile found placed: give up the resend, keep the exits.
    pub fn into_exits(self) -> Exits {
        Exits {
            symbol: self.intent.symbol().to_string(),
            stop: self.intent.stop(),
            target: self.intent.target(),
            mode: self.intent.mode(),
            approved: self.intent.qty(),
            entry_id: self.id,
        }
    }
}

/// A failed bracket submit. Only Unknown hands the Bracket back, so the only possible retry
/// reuses the same client id, which Alpaca refuses as a duplicate if the first one landed.
#[derive(Debug)]
pub enum BracketError {
    /// Invalid or Rejected: nothing was placed and the intent is spent.
    Failed(SubmitError),
    Unknown(String, Bracket),
}

/// What a placed bracket's exits are, for an OCO after a partial fill. Not Clone: only a
/// placed bracket yields one.
#[derive(Debug, PartialEq, Eq)]
pub struct Exits {
    symbol: String,
    stop: Price,
    target: Price,
    mode: crate::config::Mode,
    approved: Qty,
    entry_id: String,
}

impl Exits {
    pub fn entry_id(&self) -> &str {
        &self.entry_id
    }
    pub fn oco_id(&self) -> String {
        format!("{}{OCO_SUFFIX}", self.entry_id)
    }
}

/// The filled shares of `entry`, if it's this bracket's entry and within what the gate approved.
fn oco_qty(x: &Exits, entry: &Order) -> Result<Qty, SubmitError> {
    if entry.client_order_id != x.entry_id || entry.symbol != x.symbol {
        return Err(SubmitError::Invalid(format!(
            "OCO exits are for {} {}, order is {} {}",
            x.entry_id, x.symbol, entry.client_order_id, entry.symbol
        )));
    }
    match Qty::new(entry.filled_qty) {
        Some(q) if q <= x.approved => Ok(q),
        _ => Err(SubmitError::Invalid(format!(
            "refusing OCO for {} filled shares, approved {:?}",
            entry.filled_qty, x.approved
        ))),
    }
}

/// The result of a cancel request.
#[derive(Debug, PartialEq, Eq)]
pub enum Cancel {
    /// Alpaca accepted the cancel; confirm with the order's status, it may still fill first.
    Requested,
    /// Alpaca's id is no longer cancelable-by-id: the order it named is gone.
    Gone,
    /// Our client id isn't found *now*. Not proof it never landed (indexing lag after an
    /// Unknown submit): keep the entry unresolved and re-poll open orders and positions.
    NotFound,
    /// 422: already filled, cancelled or otherwise final. Reconcile its status.
    NotCancelable,
}

fn cancel_outcome(status: u16, body: &str) -> Result<Cancel, String> {
    match status {
        200..300 => Ok(Cancel::Requested),
        422 => Ok(Cancel::NotCancelable),
        _ if is_not_found(status, body) => Ok(Cancel::Gone),
        _ => Err(format!("Alpaca cancel HTTP {status}: {}", clip(body))),
    }
}

/// Alpaca's id to cancel, or `NotFound` when our client id lookup came back empty.
fn cancel_target(found: Option<Order>) -> Result<String, Cancel> {
    found.map(|o| o.id).ok_or(Cancel::NotFound)
}

/// Alpaca takes prices as decimal strings; ours are whole cents.
fn decimal(p: Price) -> String {
    format!("{}.{:02}", p.cents() / 100, p.cents() % 100)
}

fn bracket_body(id: &str, o: &OrderIntent) -> Value {
    json!({
        "symbol": o.symbol(),
        "qty": o.qty().get().to_string(),
        "side": "buy",
        "type": "limit",
        "time_in_force": "day",
        "limit_price": decimal(o.entry()),
        "extended_hours": false,
        "order_class": "bracket",
        "client_order_id": id,
        "take_profit": { "limit_price": decimal(o.target()) },
        "stop_loss": { "stop_price": decimal(o.stop()) },
    })
}

/// Exits for a partially filled entry, at the same stop and target the gate approved. [verify OCO]
fn oco_body(x: &Exits, filled: Qty) -> Value {
    json!({
        "symbol": x.symbol,
        "qty": filled.get().to_string(),
        "side": "sell",
        "type": "limit",
        "time_in_force": "day",
        "order_class": "oco",
        "client_order_id": x.oco_id(),
        "take_profit": { "limit_price": decimal(x.target) },
        "stop_loss": { "stop_price": decimal(x.stop) },
    })
}

/// One entry of the multi-status reply to "close all positions".
#[derive(Debug, Deserialize)]
struct Closed {
    symbol: String,
    status: u16,
    #[serde(default)]
    body: Value,
}

fn check_closed(body: &str) -> Result<(), String> {
    let all: Vec<Closed> = serde_json::from_str(body)
        .map_err(|e| format!("unexpected close-all response from Alpaca: {e}"))?;
    let failed: Vec<String> = all
        .iter()
        .filter(|c| !(200..300).contains(&c.status))
        .map(|c| {
            let why = c.body.to_string();
            format!("{} (HTTP {}: {})", c.symbol, c.status, clip(&why))
        })
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("close-all failed for {}", failed.join(", ")))
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

fn price<'de, D: Deserializer<'de>>(d: D) -> Result<Price, D::Error> {
    Price::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
}

fn opt_price<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Price>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|s| Price::parse(&s).map_err(serde::de::Error::custom))
        .transpose()
}

// shortcut: whole shares only; fractional quantities (crypto) fail to parse, revisit when crypto lands.
fn shares<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    String::deserialize(d)?
        .parse()
        .map_err(serde::de::Error::custom)
}

fn signed_shares<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    String::deserialize(d)?
        .parse()
        .map_err(serde::de::Error::custom)
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

    /// A read-only GET.
    pub fn get<T: DeserializeOwned>(&self, api: Api, path: &str) -> Result<T, String> {
        let (status, body) = self.send(Method::GET, api, path, None)?;
        if !(200..300).contains(&status) {
            return Err(format!("Alpaca {path} HTTP {status}: {}", clip(&body)));
        }
        serde_json::from_str(&body)
            .map_err(|e| format!("unexpected response from Alpaca {path}: {e}"))
    }

    pub fn positions(&self) -> Result<Vec<Position>, String> {
        self.get(Api::Trading, "/v2/positions")
    }

    /// The order with our id, with its legs, or None only on Alpaca's own "order not found".
    /// None doesn't prove an Unknown submit never landed (Alpaca may not have indexed it yet):
    /// a caller resolving one must also check open orders and positions for that symbol.
    pub fn order_by_client_id(&self, id: &str) -> Result<Option<Order>, String> {
        if !valid_client_id(id) {
            return Err(format!("refusing client_order_id {id:?}"));
        }
        let path = format!("/v2/orders:by_client_order_id?client_order_id={id}&nested=true");
        let (status, body) = self.send(Method::GET, Api::Trading, &path, None)?;
        match status {
            200..300 => serde_json::from_str(&body)
                .map(Some)
                .map_err(|e| format!("unexpected order from Alpaca: {e}")),
            _ if is_not_found(status, &body) => Ok(None),
            _ => Err(format!(
                "Alpaca order lookup HTTP {status}: {}",
                clip(&body)
            )),
        }
    }

    /// The only way to open a position: limit entry plus broker-held stop and target. Consumes
    /// the Bracket; on Unknown it comes back, for a resend under the same id once reconciled.
    pub fn submit_bracket(&self, b: Bracket) -> Result<(Order, Exits), BracketError> {
        if !valid_entry_id(&b.id) {
            return Err(BracketError::Failed(SubmitError::Invalid(format!(
                "refusing client_order_id {:?}",
                b.id
            ))));
        }
        match self.submit(b.intent.mode(), &b.id, bracket_body(&b.id, &b.intent)) {
            Ok(order) => Ok((order, b.into_exits())),
            Err(SubmitError::Unknown(m)) => Err(BracketError::Unknown(m, b)),
            Err(e) => Err(BracketError::Failed(e)),
        }
    }

    /// Protects a partially filled entry once its remainder is cancelled (DESIGN §8), for
    /// exactly the shares Alpaca reports filled on `entry`. The OCO id is fixed per entry, so a
    /// resend after Unknown is safe. Rejected (e.g. price already through the stop) leaves the
    /// shares unprotected: `close_position` them now.
    pub fn submit_oco(&self, x: &Exits, entry: &Order) -> Result<Order, SubmitError> {
        let filled = oco_qty(x, entry)?;
        self.submit(x.mode, &x.oco_id(), oco_body(x, filled))
    }

    fn submit(
        &self,
        mode: crate::config::Mode,
        id: &str,
        body: Value,
    ) -> Result<Order, SubmitError> {
        // Limits approved for paper must never size a live order, or the reverse.
        if mode != self.cfg.mode() {
            return Err(SubmitError::Invalid(format!(
                "order approved under {mode:?} limits but the broker is {:?}",
                self.cfg.mode()
            )));
        }
        if !valid_client_id(id) {
            return Err(SubmitError::Invalid(format!(
                "refusing client_order_id {id:?}"
            )));
        }
        let (status, text) = self.request(Method::POST, Api::Trading, "/v2/orders", Some(&body))?;
        if !(200..300).contains(&status) {
            return Err(classify(status, &text));
        }
        // Accepted, so the order exists even if we can't read the reply.
        serde_json::from_str(&text)
            .map_err(|e| SubmitError::Unknown(format!("order accepted, reply unreadable: {e}")))
    }

    /// Cancel by Alpaca's order id (hex UUID).
    pub fn cancel(&self, order_id: &str) -> Result<Cancel, String> {
        if order_id.is_empty() || !order_id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return Err(format!("refusing order id {order_id:?}"));
        }
        let path = format!("/v2/orders/{order_id}");
        let (status, body) = self.send(Method::DELETE, Api::Trading, &path, None)?;
        cancel_outcome(status, &body)
    }

    /// Cancel by our client id (what the engine holds): look it up, then cancel.
    /// A lookup miss is `NotFound`, never `Gone`: see that variant.
    pub fn cancel_by_client_id(&self, id: &str) -> Result<Cancel, String> {
        match cancel_target(self.order_by_client_id(id)?) {
            Ok(order_id) => self.cancel(&order_id),
            Err(miss) => Ok(miss),
        }
    }

    /// Market-closes one position. Ok means Alpaca accepted the close order, not that it's
    /// flat: confirm with `positions()`.
    pub fn close_position(&self, symbol: &str) -> Result<(), String> {
        if !valid_symbol(symbol) {
            return Err(format!("refusing symbol {symbol:?}"));
        }
        let path = format!("/v2/positions/{symbol}");
        let (status, body) = self.send(Method::DELETE, Api::Trading, &path, None)?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "Alpaca close {symbol} HTTP {status}: {}",
                clip(&body)
            ));
        }
        Ok(())
    }

    /// Closes every position and cancels its bracket legs first, which avoids wash-trade
    /// rejections. [verify cancel_orders] Ok means every close was accepted, not that the
    /// account is flat: confirm with `positions()`.
    pub fn close_all(&self) -> Result<(), String> {
        let path = "/v2/positions?cancel_orders=true";
        let (status, body) = self.send(Method::DELETE, Api::Trading, path, None)?;
        if !(200..300).contains(&status) {
            return Err(format!("Alpaca close-all HTTP {status}: {}", clip(&body)));
        }
        check_closed(&body)
    }

    /// `send` for reads and cancels, where "not sent" and "unknown" both just mean error.
    fn send(
        &self,
        method: Method,
        api: Api,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, String), String> {
        self.request(method, api, path, body)
            .map_err(|e| e.to_string())
    }

    /// The one place a request is made: path check, keys, and the raw status and body.
    /// Invalid: refused before sending. Unknown: it may have reached Alpaca.
    fn request(
        &self,
        method: Method,
        api: Api,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, String), SubmitError> {
        // "@host/x" after the base would make `host` the real destination and hand it the keys;
        // "/.." would let the URL parser climb to another endpoint.
        let unsafe_path = !path.starts_with('/')
            || ["@", "\\", "#", "://", "/."]
                .iter()
                .any(|bad| path.contains(bad))
            || (method != Method::GET && api != Api::Trading);
        if unsafe_path {
            return Err(SubmitError::Invalid(format!(
                "refusing Alpaca path {path:?}"
            )));
        }
        let base = match api {
            Api::Trading => self.cfg.mode().base_url(),
            Api::MarketData => "https://data.alpaca.markets",
        };
        let mut secret = HeaderValue::from_str(self.cfg.secret()).map_err(|_| {
            SubmitError::Invalid(
                "Alpaca secret contains characters not allowed in a header".to_string(),
            )
        })?;
        secret.set_sensitive(true);
        let mut req = self
            .http
            .request(method, format!("{base}{path}"))
            .header("APCA-API-KEY-ID", self.cfg.key_id())
            .header("APCA-API-SECRET-KEY", secret);
        if let Some(b) = body {
            req = req.json(b);
        }
        // shortcut: any transport error counts as maybe-sent; a connect error is provably not, split it if retries need that.
        let resp = req
            .send()
            .map_err(|e| SubmitError::Unknown(format!("request to Alpaca {path} failed: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(|e| {
            SubmitError::Unknown(format!(
                "reading Alpaca {path} response (HTTP {status}): {e}"
            ))
        })?;
        Ok((status, text))
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
    fn paths_that_could_redirect_the_keys_are_refused_before_any_request() {
        let cfg = crate::config::from_env(|k| match k {
            "ALPACA_PAPER_KEY_ID" => Some("pk".into()),
            "ALPACA_PAPER_SECRET" => Some("ps".into()),
            _ => None,
        })
        .unwrap();
        let a = Alpaca::new(cfg).unwrap();
        for bad in [
            "@evil.example/x",
            "v2/account",
            "/x@evil.example",
            "/a\\b",
            "/a#b",
            "/https://evil",
            "/v2/positions/..",
            "/v2/./account",
        ] {
            let err = a
                .get::<serde_json::Value>(Api::MarketData, bad)
                .unwrap_err();
            assert!(err.contains("refusing"), "{bad}: {err}");
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

    fn intent(mode: crate::config::Mode) -> OrderIntent {
        use crate::risk::{Limits, RiskState, gate};
        let p = |c| Price::from_cents(c).unwrap();
        let c =
            crate::strategy::Candidate::long("SPY", p(10_000), p(9_900), p(10_200), String::new())
                .unwrap();
        let s = RiskState {
            entries_allowed: true,
            buying_power: Usd::from_cents(1_000_000_000),
            ..RiskState::default()
        };
        gate(
            &c,
            crate::vetting::Verdict::Approve,
            &s,
            &Limits::for_mode(mode),
        )
        .unwrap()
    }

    fn paper_client() -> Alpaca {
        let cfg = crate::config::from_env(|k| match k {
            "ALPACA_PAPER_KEY_ID" => Some("pk".into()),
            "ALPACA_PAPER_SECRET" => Some("ps".into()),
            _ => None,
        })
        .unwrap();
        Alpaca::new(cfg).unwrap()
    }

    #[test]
    fn bracket_body_is_a_day_limit_buy_with_broker_held_exits() {
        let o = intent(crate::config::Mode::Paper);
        let b = bracket_body("bmv-2024-01-02-SPY-1", &o);
        assert_eq!(b["order_class"], "bracket");
        assert_eq!(b["side"], "buy");
        assert_eq!(b["type"], "limit");
        assert_eq!(b["time_in_force"], "day");
        assert_eq!(b["extended_hours"], false);
        assert_eq!(b["qty"], o.qty().get().to_string());
        assert_eq!(b["limit_price"], "100.00");
        assert_eq!(b["stop_loss"]["stop_price"], "99.00");
        assert_eq!(b["take_profit"]["limit_price"], "102.00");
        assert_eq!(b["client_order_id"], "bmv-2024-01-02-SPY-1");
    }

    const ID: &str = "bmv-2024-01-02-SPY-1";

    fn exits() -> Exits {
        Bracket::new(ID.into(), intent(crate::config::Mode::Paper)).into_exits()
    }

    fn entry_filled(n: &str) -> Order {
        serde_json::from_str(
            &BRACKET_ORDER.replace(r#""filled_qty":"10""#, &format!(r#""filled_qty":"{n}""#)),
        )
        .unwrap()
    }

    #[test]
    fn oco_body_sells_only_the_filled_shares_at_the_approved_exits() {
        let b = oco_body(&exits(), Qty::new(3).unwrap());
        assert_eq!(b["order_class"], "oco");
        assert_eq!(b["side"], "sell");
        assert_eq!(b["qty"], "3");
        assert_eq!(b["client_order_id"], "bmv-2024-01-02-SPY-1-x");
        assert_eq!(b["stop_loss"]["stop_price"], "99.00");
        assert_eq!(b["take_profit"]["limit_price"], "102.00");
    }

    #[test]
    fn oco_qty_is_alpacas_filled_qty_within_the_approval() {
        let x = exits();
        assert_eq!(oco_qty(&x, &entry_filled("10")), Ok(Qty::new(10).unwrap()));
        assert_eq!(
            oco_qty(&x, &entry_filled(&x.approved.get().to_string())),
            Ok(x.approved)
        );
        let over = (x.approved.get() + 1).to_string();
        for n in ["0", over.as_str()] {
            assert!(
                matches!(oco_qty(&x, &entry_filled(n)), Err(SubmitError::Invalid(_))),
                "{n}"
            );
        }
        let other =
            serde_json::from_str::<Order>(&BRACKET_ORDER.replace(ID, "bmv-2024-01-02-SPY-2"))
                .unwrap();
        assert!(matches!(oco_qty(&x, &other), Err(SubmitError::Invalid(_))));
        let sym = serde_json::from_str::<Order>(&BRACKET_ORDER.replace(
            r#""symbol":"SPY","side":"buy""#,
            r#""symbol":"QQQ","side":"buy""#,
        ))
        .unwrap();
        assert!(matches!(oco_qty(&x, &sym), Err(SubmitError::Invalid(_))));
    }

    #[test]
    fn client_ids_must_have_our_exact_shape() {
        for good in [ID, "bmv-2024-01-02-BRK.B-123", "bmv-2024-01-02-SPY-1-x"] {
            assert!(valid_client_id(good), "{good}");
        }
        assert!(
            !valid_entry_id("bmv-2024-01-02-SPY-1-x"),
            "OCO id is not an entry id"
        );
        for bad in [
            "",
            "bmv-d-SPY-1",
            "bmv-2024-1-02-SPY-1",
            "bmv-2024-01-02-spy-1",
            "bmv-2024-01-02-SPY-",
            "bmv-2024-01-02-SPY-1a",
            "bmv-2024-01-02--1",
            "bmv-2024-01-02-SPY-1-y",
            "bmv-2024-01-02-SPY-1-x-x",
            "bmv-2024-01-02-SPY/1-1",
            "bmv-2024-01-02-SPY-1234567890",
            "bmv-2024-01-02-ABCDEFGHIJK-1",
            "xbmv-2024-01-02-SPY-1",
        ] {
            assert!(!valid_client_id(bad), "{bad}");
        }
    }

    #[test]
    fn only_alpacas_not_found_json_means_absent() {
        assert!(is_not_found(
            404,
            r#"{"code":40410000,"message":"order not found"}"#
        ));
        assert!(!is_not_found(404, "<html>Not Found</html>"));
        assert!(!is_not_found(404, r#"{"code":40010001,"message":"x"}"#));
        assert!(!is_not_found(200, r#"{"code":40410000}"#));
    }

    #[test]
    fn cancel_outcomes_are_typed() {
        let nf = r#"{"code":40410000,"message":"order not found"}"#;
        assert_eq!(cancel_outcome(204, ""), Ok(Cancel::Requested));
        assert_eq!(cancel_outcome(404, nf), Ok(Cancel::Gone));
        assert_eq!(cancel_outcome(422, "{}"), Ok(Cancel::NotCancelable));
        assert!(cancel_outcome(404, "proxy").is_err());
        assert!(cancel_outcome(500, "").is_err());
    }

    #[test]
    fn client_id_lookup_miss_is_not_found_not_gone() {
        assert_eq!(cancel_target(None), Err(Cancel::NotFound));
    }

    #[test]
    fn submit_error_displays_its_message() {
        assert_eq!(SubmitError::Invalid("m".into()).to_string(), "m");
    }

    #[test]
    fn decimal_prints_whole_cents() {
        for (c, want) in [
            (1, "0.01"),
            (105, "1.05"),
            (10_000, "100.00"),
            (123_456, "1234.56"),
        ] {
            assert_eq!(decimal(Price::from_cents(c).unwrap()), want);
        }
    }

    #[test]
    fn only_a_definite_refusal_counts_as_rejected() {
        use SubmitError::*;
        let bp = r#"{"code":40310000,"message":"insufficient buying power"}"#;
        let bad = r#"{"code":42210000,"message":"invalid limit_price"}"#;
        let dup = r#"{"code":42210000,"message":"client_order_id must be unique"}"#;
        assert!(matches!(classify(403, bp), Rejected(_)));
        assert!(matches!(classify(422, bad), Rejected(_)));
        assert!(matches!(classify(422, dup), Unknown(_)));
        // a known code on the wrong status, plain text, or an unknown code: not definite
        assert!(matches!(classify(500, bp), Unknown(_)));
        assert!(matches!(
            classify(403, "insufficient buying power"),
            Unknown(_)
        ));
        assert!(matches!(
            classify(422, r#"{"code":40010001,"message":"x"}"#),
            Unknown(_)
        ));
        for s in [408, 429, 500, 502, 503, 504] {
            assert!(matches!(classify(s, ""), Unknown(_)), "{s}");
        }
    }

    #[test]
    fn orders_are_refused_before_any_request_when_unsafe() {
        let a = paper_client();
        let live = Bracket::new(ID.into(), intent(crate::config::Mode::Live));
        let err = a.submit_bracket(live).unwrap_err();
        assert!(
            matches!(err, BracketError::Failed(SubmitError::Invalid(ref m)) if m.contains("Live")),
            "{err:?}"
        );

        let long = format!("bmv-{}", "x".repeat(70));
        for bad in [
            "",
            "SPY-1",
            "bmv-a/b",
            "bmv-a?x=1",
            "bmv-a&b",
            &long,
            "bmv-2024-01-02-SPY-1-x",
        ] {
            let b = Bracket::new(bad.into(), intent(crate::config::Mode::Paper));
            let err = a.submit_bracket(b).unwrap_err();
            assert!(
                matches!(err, BracketError::Failed(SubmitError::Invalid(_))),
                "{bad}: {err:?}"
            );
        }
        for bad in ["", "SPY-1", "bmv-a/b", "bmv-a?x=1", "bmv-a&b", &long] {
            assert!(a.order_by_client_id(bad).is_err(), "{bad}");
            assert!(a.cancel_by_client_id(bad).is_err(), "{bad}");
        }

        let err = a.submit_oco(&exits(), &entry_filled("0")).unwrap_err();
        assert!(matches!(err, SubmitError::Invalid(_)), "{err:?}");

        for bad in ["", "../positions", "abc?x", "g00d"] {
            assert!(a.cancel(bad).unwrap_err().contains("refusing"), "{bad}");
        }
        for bad in ["", "spy", "../orders", "SPY?x=1", "SPY/1", "ABCDEFGHIJK"] {
            assert!(
                a.close_position(bad).unwrap_err().contains("refusing"),
                "{bad}"
            );
        }
    }

    #[test]
    fn an_unknown_bracket_hands_back_the_same_id_and_into_exits_ends_it() {
        let b = Bracket::new(ID.into(), intent(crate::config::Mode::Paper));
        assert_eq!(b.client_id(), ID);
        let x = b.into_exits();
        assert_eq!(
            (x.entry_id(), x.oco_id().as_str()),
            (ID, "bmv-2024-01-02-SPY-1-x")
        );
        assert_eq!((x.stop.cents(), x.target.cents()), (9_900, 10_200));
    }

    // Shapes from Alpaca's API reference; replace with recorded paper replies at the [verify] step.
    const BRACKET_ORDER: &str = r#"{"id":"61e69015-8549-4bfd-b9c3-01e75843f47d",
        "client_order_id":"bmv-2024-01-02-SPY-1","symbol":"SPY","side":"buy","status":"partially_filled",
        "qty":"24","filled_qty":"10","filled_avg_price":"100.01","order_class":"bracket","legs":[
        {"id":"b9a2c3f4-0000-4000-8000-000000000001","client_order_id":"leg1","symbol":"SPY",
         "side":"sell","status":"held","qty":"24","filled_qty":"0","filled_avg_price":null}]}"#;

    #[test]
    fn parses_an_order_with_legs() {
        let o: Order = serde_json::from_str(BRACKET_ORDER).unwrap();
        assert_eq!((o.qty, o.filled_qty), (24, 10));
        assert_eq!(o.filled_avg_price, Price::from_cents(10_001));
        let legs = o.legs.unwrap();
        assert_eq!(legs[0].status, "held");
        assert_eq!(legs[0].filled_avg_price, None);
    }

    #[test]
    fn fractional_or_missing_quantities_are_errors() {
        let frac = BRACKET_ORDER.replace(r#""filled_qty":"10""#, r#""filled_qty":"1.5""#);
        assert!(serde_json::from_str::<Order>(&frac).is_err());
        let missing = BRACKET_ORDER.replace(r#""filled_qty":"10","#, "");
        assert!(serde_json::from_str::<Order>(&missing).is_err());
    }

    #[test]
    fn parses_positions_including_an_unexpected_short() {
        let p: Vec<Position> = serde_json::from_str(
            r#"[{"symbol":"SPY","qty":"10","avg_entry_price":"100.01","side":"long"},
                {"symbol":"AMD","qty":"-5","avg_entry_price":"150.00","side":"short"}]"#,
        )
        .unwrap();
        assert_eq!((p[0].qty, p[1].qty), (10, -5));
        assert_eq!(p[1].avg_entry_price, Price::from_cents(15_000).unwrap());
    }

    #[test]
    fn close_all_fails_if_any_symbol_failed() {
        assert!(check_closed("[]").is_ok());
        assert!(check_closed(r#"[{"symbol":"SPY","status":200,"body":{}}]"#).is_ok());
        let err = check_closed(
            r#"[{"symbol":"SPY","status":200,"body":{}},{"symbol":"AMD","status":403,"body":{}}]"#,
        )
        .unwrap_err();
        assert!(
            err.contains("AMD (HTTP 403: {})") && !err.contains("SPY"),
            "{err}"
        );
        let why = check_closed(r#"[{"symbol":"AMD","status":403,"body":{"message":"held"}}]"#);
        assert!(why.unwrap_err().contains("held"));
        assert!(check_closed("not json").is_err());
    }
}
