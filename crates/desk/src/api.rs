//! The Desk API (DESIGN §4c, S30): JSON over HTTP on 127.0.0.1, one bearer token per agent. The
//! token names the agent; its role always comes from the store, so an agent can't claim another.
//! Refusals come back with their reason; store faults come back as a bare 500 and are logged here.
//! There is no way to become the owner over HTTP: `desk init` registers the owner in-process, as
//! the user that owns the database.

use crate::DeskError;
use crate::lifecycle::{HaltReason, State};
use crate::owner::OwnerAction;
use crate::permissions::Role;
use crate::store::Store;
use axum::extract::{DefaultBodyLimit, Path, Request, State as Ctx};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;

pub const DEFAULT_ADDR: &str = "127.0.0.1:8790";

type Desk = Ctx<Arc<Store>>;
type Reply = std::result::Result<Json<Value>, ApiError>;

pub struct ApiError(DeskError);

impl From<DeskError> for ApiError {
    fn from(e: DeskError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match &self.0 {
            DeskError::Denied(_) | DeskError::Unsigned(_) => StatusCode::FORBIDDEN,
            DeskError::Illegal(_) => StatusCode::CONFLICT,
            DeskError::Budget(_) => StatusCode::PAYMENT_REQUIRED,
            DeskError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => {
                eprintln!("desk: {}", self.0);
                let body = Json(json!({"error": "internal error; see the desk log"}));
                return (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
            }
        };
        (code, Json(json!({"error": self.0.to_string()}))).into_response()
    }
}

fn denied(m: &str) -> ApiError {
    ApiError(DeskError::Denied(m.into()))
}

/// The agent the request's bearer token belongs to.
async fn caller(store: &Store, headers: &HeaderMap) -> std::result::Result<String, ApiError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| denied("a bearer token is required"))?;
    Ok(store.agent_for_token(token).await?)
}

async fn board(store: &Store, headers: &HeaderMap) -> std::result::Result<String, ApiError> {
    let agent = caller(store, headers).await?;
    if store.role(&agent).await? != Role::Board {
        return Err(denied("only the owner"));
    }
    Ok(agent)
}

fn state(s: &str) -> std::result::Result<State, ApiError> {
    State::parse(s).ok_or_else(|| denied(&format!("no state {s:?}")))
}

/// Refuses any Host but loopback, so a web page can't reach the desk by DNS rebinding.
async fn loopback_only(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split(':').next());
    if !matches!(host, Some("127.0.0.1" | "localhost")) {
        let body = Json(json!({"error": "the desk answers to 127.0.0.1 only"}));
        return (StatusCode::MISDIRECTED_REQUEST, body).into_response();
    }
    next.run(req).await
}

async fn whoami(Ctx(s): Desk, h: HeaderMap) -> Reply {
    let agent = caller(&s, &h).await?;
    let role = s.role(&agent).await?;
    Ok(Json(json!({"agent": agent, "role": role})))
}

async fn strategies(Ctx(s): Desk, h: HeaderMap) -> Reply {
    caller(&s, &h).await?;
    let rows: Vec<Value> = s
        .strategies()
        .await?
        .into_iter()
        .map(|(id, name, state, version)| {
            json!({"id": id, "name": name, "state": state, "version": version})
        })
        .collect();
    Ok(Json(json!(rows)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
}

async fn create(Ctx(s): Desk, h: HeaderMap, Json(b): Json<Create>) -> Reply {
    let agent = caller(&s, &h).await?;
    Ok(Json(
        json!({"id": s.create_strategy(&agent, &b.name).await?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Criteria {
    criteria: String,
}

async fn criteria(
    Ctx(s): Desk,
    h: HeaderMap,
    Path(id): Path<i64>,
    Json(b): Json<Criteria>,
) -> Reply {
    let agent = caller(&s, &h).await?;
    s.set_criteria(&agent, id, &b.criteria).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    code_hash: String,
}

async fn version(Ctx(s): Desk, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<Version>) -> Reply {
    let agent = caller(&s, &h).await?;
    Ok(Json(
        json!({"version": s.submit_version(&agent, id, &b.code_hash).await?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Backtest {
    official: bool,
    inputs_hash: String,
    results_hash: String,
}

async fn backtest(
    Ctx(s): Desk,
    h: HeaderMap,
    Path(id): Path<i64>,
    Json(b): Json<Backtest>,
) -> Reply {
    let agent = caller(&s, &h).await?;
    let reproduces = s
        .record_backtest(&agent, id, b.official, &b.inputs_hash, &b.results_hash)
        .await?;
    Ok(Json(
        json!({"reproduces": reproduces, "state": s.state(id).await?.as_str()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Paper {
    days: u32,
    trades: u32,
}

async fn paper(Ctx(s): Desk, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<Paper>) -> Reply {
    let agent = caller(&s, &h).await?;
    s.record_paper(&agent, id, b.days, b.trades).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sign {
    to: String,
}

async fn sign(Ctx(s): Desk, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<Sign>) -> Reply {
    let agent = caller(&s, &h).await?;
    let moved = s.sign(&agent, id, state(&b.to)?).await?;
    Ok(Json(
        json!({"moved": moved, "state": s.state(id).await?.as_str()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Halt {
    reason: String,
}

async fn halt(Ctx(s): Desk, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<Halt>) -> Reply {
    let agent = caller(&s, &h).await?;
    let reason =
        HaltReason::parse(&b.reason).ok_or_else(|| denied(&format!("no reason {:?}", b.reason)))?;
    s.halt(&agent, id, reason).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limit {
    name: String,
    value: String,
}

async fn limits(Ctx(s): Desk, h: HeaderMap, Path(id): Path<i64>) -> Reply {
    caller(&s, &h).await?;
    let rows: Vec<Value> = s
        .limits(id)
        .await?
        .into_iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    Ok(Json(json!(rows)))
}

/// Tightening only; loosening is an owner request.
async fn limit(Ctx(s): Desk, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<Limit>) -> Reply {
    let agent = caller(&s, &h).await?;
    let value = Decimal::from_str(&b.value).map_err(|_| denied("not an amount"))?;
    s.set_limit(&agent, id, &b.name, value, None).await?;
    Ok(Json(json!({"ok": true})))
}

async fn pending(Ctx(s): Desk, h: HeaderMap) -> Reply {
    caller(&s, &h).await?;
    Ok(Json(json!(s.pending_owner_actions().await?)))
}

async fn request(Ctx(s): Desk, h: HeaderMap, Json(action): Json<OwnerAction>) -> Reply {
    let agent = caller(&s, &h).await?;
    Ok(Json(json!(s.request_owner_action(&agent, action).await?)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approve {
    signature: String,
}

/// The owner's token and their Touch ID signature, both.
async fn approve(
    Ctx(s): Desk,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Approve>,
) -> Reply {
    board(&s, &h).await?;
    let token = s.approve_owner_action(&id, &b.signature).await?;
    Ok(Json(json!({"ok": true, "token": token})))
}

async fn verify(Ctx(s): Desk, h: HeaderMap) -> Reply {
    board(&s, &h).await?;
    Ok(Json(json!({"entries": s.verify_audit().await?})))
}

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/strategies", get(strategies).post(create))
        .route("/v1/strategies/{id}/criteria", post(criteria))
        .route("/v1/strategies/{id}/versions", post(version))
        .route("/v1/strategies/{id}/backtests", post(backtest))
        .route("/v1/strategies/{id}/paper", post(paper))
        .route("/v1/strategies/{id}/sign", post(sign))
        .route("/v1/strategies/{id}/halt", post(halt))
        .route("/v1/strategies/{id}/limits", get(limits).post(limit))
        .route("/v1/owner/requests", get(pending).post(request))
        .route("/v1/owner/requests/{id}/approve", post(approve))
        .route("/v1/audit/verify", get(verify))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(loopback_only))
        .with_state(store)
}

/// Serves the desk on a listener the caller bound (to 127.0.0.1 only).
pub async fn serve(listener: tokio::net::TcpListener, store: Store) -> std::io::Result<()> {
    axum::serve(listener, router(Arc::new(store))).await
}

/// A blocking client for the owner's `desk` command: plain HTTP/1.1 to the loopback server.
/// Returns the JSON body, or the desk's refusal as text.
pub fn call(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> std::result::Result<Value, String> {
    use std::io::{Read, Write};
    use std::time::Duration;
    if token.is_some_and(|t| !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')) {
        return Err("that token isn't a desk token".into());
    }
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    let reach = |e: std::io::Error| format!("can't reach the desk at {addr}: {e}");
    let sock = addr
        .parse()
        .map_err(|_| format!("{addr} isn't an ip:port address"))?;
    let mut stream =
        std::net::TcpStream::connect_timeout(&sock, Duration::from_secs(5)).map_err(reach)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(30))))
        .map_err(reach)?;
    let mut out = String::new();
    stream
        .write_all(req.as_bytes())
        .and_then(|_| stream.read_to_string(&mut out))
        .map_err(|e| format!("desk connection: {e}"))?;
    let (head, rest) = out
        .split_once("\r\n\r\n")
        .ok_or("the desk sent a bad reply")?;
    let status = head.split(' ').nth(1).unwrap_or("");
    let value: Value = serde_json::from_str(rest).unwrap_or_else(|_| json!({"error": rest}));
    if status.starts_with('2') {
        Ok(value)
    } else {
        Err(value["error"].as_str().unwrap_or(rest).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::HumanProof;

    async fn up() -> (String, Arc<Store>) {
        let store = Store::open("sqlite::memory:").await.unwrap();
        store.bootstrap_owner("test-key").await.unwrap();
        for (id, role) in [("res-1", Role::Researcher), ("strat-1", Role::Strategist)] {
            store
                .add_agent("board", id, role, Some(HumanProof::for_test()))
                .await
                .unwrap();
        }
        // the test keeps a handle on the store, so it can issue tokens
        let store = Arc::new(store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = router(store.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (addr, store)
    }

    async fn blocking(
        addr: &str,
        method: &'static str,
        path: &str,
        token: Option<String>,
        body: Option<Value>,
    ) -> std::result::Result<Value, String> {
        let (addr, path) = (addr.to_string(), path.to_string());
        tokio::task::spawn_blocking(move || {
            call(&addr, method, &path, token.as_deref(), body.as_ref())
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_token_names_the_agent_and_the_store_decides() {
        let (addr, store) = up().await;
        let res = store.issue_token("res-1").await.unwrap();
        let strat = store.issue_token("strat-1").await.unwrap();

        let me = blocking(&addr, "GET", "/v1/whoami", Some(res.clone()), None)
            .await
            .unwrap();
        assert_eq!(me["role"], "researcher");
        assert!(
            blocking(&addr, "GET", "/v1/whoami", None, None)
                .await
                .unwrap_err()
                .contains("bearer")
        );
        assert!(
            blocking(&addr, "GET", "/v1/whoami", Some("desk_x".into()), None)
                .await
                .is_err()
        );

        let made = blocking(
            &addr,
            "POST",
            "/v1/strategies",
            Some(res.clone()),
            Some(json!({"name": "gap fade"})),
        )
        .await
        .unwrap();
        let id = made["id"].as_i64().unwrap();
        // the researcher can't approve its own hypothesis; the strategist can
        let path = format!("/v1/strategies/{id}/sign");
        let to = json!({"to": "hypothesis_approved"});
        assert!(
            blocking(&addr, "POST", &path, Some(res.clone()), Some(to.clone()))
                .await
                .is_err()
        );
        let r = blocking(&addr, "POST", &path, Some(strat), Some(to))
            .await
            .unwrap();
        assert_eq!(r["state"], "hypothesis_approved");

        // owner-only routes refuse an agent's token, and extra fields are refused
        assert!(
            blocking(&addr, "GET", "/v1/audit/verify", Some(res.clone()), None)
                .await
                .is_err()
        );
        assert!(
            blocking(
                &addr,
                "POST",
                "/v1/strategies",
                Some(res.clone()),
                Some(json!({"name": "x", "role": "board"}))
            )
            .await
            .is_err()
        );
        // no owner over HTTP, and a rebinding page's Host is refused
        let init = json!({"public": "00", "signature": "00"});
        assert!(
            blocking(&addr, "POST", "/v1/init", None, Some(init))
                .await
                .is_err()
        );
        let raw = addr.clone();
        let reply = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let mut c = std::net::TcpStream::connect(&raw).unwrap();
            c.write_all(
                b"GET /v1/whoami HTTP/1.1\r\nHost: evil.example:8790\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
            let mut out = String::new();
            c.read_to_string(&mut out).unwrap();
            out
        })
        .await
        .unwrap();
        assert!(reply.starts_with("HTTP/1.1 421"), "{reply}");

        // the agents' MCP tools reach the same desk with the same token
        let (addr2, res2) = (addr.clone(), res.clone());
        let r = tokio::task::spawn_blocking(move || {
            let desk = |m: &str, p: &str, b: Option<&Value>| call(&addr2, m, p, Some(&res2), b);
            let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": "whoami", "arguments": {}}});
            crate::mcp::handle(&msg, &desk).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(r["result"]["isError"], false);
        assert!(
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("researcher")
        );
    }
}
