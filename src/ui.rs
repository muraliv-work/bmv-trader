//! Local web console (DECISION S16, S24): the same view as `watch`, plus STOP and flatten, and
//! Run-now for the launchd jobs that place no orders. There is no route to clear STOP, start the
//! trading engine or place an order. Listens on 127.0.0.1 only, behind a token minted per launch.

use crate::broker::Alpaca;
use crate::broker::Api;
use crate::safety::{self, Engaged};
use crate::{desk, live, momentum, scan, services, watch};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Loopback only, and not an option: anything on the network must not reach the controls.
pub const ADDR: &str = "127.0.0.1";
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 1024;
/// Alpaca is read once per this many seconds, however many tabs are open.
const CACHE_SECS: u64 = 3;
/// A whole request must arrive within this.
const REQUEST_SECS: u64 = 5;
const MAX_CONNECTIONS: usize = 32;

const INDEX: &str = include_str!("ui/index.html");
const HELP: &str = include_str!("ui/help.html");
const APP_JS: &str = include_str!("ui/app.js");
const APP_CSS: &str = include_str!("ui/app.css");
const SERVICES: &str = include_str!("ui/services.html");
const SERVICES_JS: &str = include_str!("ui/services.js");
const DESK: &str = include_str!("ui/desk.html");
const DESK_JS: &str = include_str!("ui/desk.js");
/// lightweight-charts 5.1.0, standalone production build (Apache-2.0; src/ui/vendor/LICENSE).
const CHART_JS: &str = include_str!("ui/vendor/lightweight-charts.js");
/// The Services checks (Keychain, launchctl, loopback HTTP) run at most once per this.
const SERVICES_SECS: u64 = 30;
/// Trial 10 progress and the equity history are read at most once per this.
const RESEARCH_SECS: u64 = 60;
/// The Quant Desk and the key table are read at most once per this (S47): the audit walk is the
/// whole chain, and the desk's connection slots are the agents'.
const DESK_SECS: u64 = 120;

/// The launchd jobs Run-now may start (DECISION S24): none of them can place an order.
pub const RUNNABLE: [&str; 3] = ["desk", "notes", "llm"];
/// What the Services buttons may act on (DECISION S28), and the actions each takes. The trading
/// engine isn't here: starting it stays terminal-only.
pub const TARGETS: [(&str, &[&str]); 5] = [
    ("ollama", &["start", "restart"]),
    ("omniroute", &["start", "restart"]),
    ("desk", &["run", "restart"]),
    ("notes", &["run", "restart"]),
    ("llm", &["run", "restart"]),
];

/// The word a person types for an action.
pub fn word(action: &str) -> &'static str {
    match action {
        "restart" => "RESTART",
        "start" => "START",
        _ => "RUN",
    }
}

/// Every route there is. A test pins it, so a new control can't slip in unreviewed.
pub const ROUTES: [(&str, &str); 19] = [
    ("GET", "/"),
    ("GET", "/help"),
    ("GET", "/services"),
    ("GET", "/desk"),
    ("GET", "/app.js"),
    ("GET", "/services.js"),
    ("GET", "/desk.js"),
    ("GET", "/app.css"),
    ("GET", "/chart.js"),
    ("GET", "/api/state"),
    ("GET", "/api/services"),
    ("GET", "/api/desk"),
    ("GET", "/api/equity"),
    ("GET", "/api/lessons"),
    ("GET", "/api/desk/snapshot"),
    ("GET", "/api/keys"),
    ("POST", "/api/stop"),
    ("POST", "/api/flatten"),
    ("POST", "/api/run"),
];

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Response {
    fn ok(content_type: &'static str, body: &str) -> Response {
        Response {
            status: 200,
            content_type,
            body: body.to_string(),
        }
    }
    fn json(status: u16, ok: bool, message: &str) -> Response {
        Response {
            status,
            content_type: "application/json",
            body: serde_json::json!({ "ok": ok, "message": message }).to_string(),
        }
    }
}

/// What `route` needs from the outside world; tests pass fakes. Shared by every connection thread.
pub struct Ctx<'a> {
    pub port: u16,
    pub token: String,
    pub stop: PathBuf,
    /// The latest snapshot as JSON, without waiting on Alpaca.
    pub state: &'a (dyn Fn() -> Result<String, String> + Sync),
    /// The latest Services report as JSON, without waiting on the checks.
    pub services: &'a (dyn Fn() -> Result<String, String> + Sync),
    /// Ask for a fresh snapshot now (after a control).
    pub refresh: &'a (dyn Fn() + Sync),
    /// Close everything and confirm flat.
    pub flatten: &'a (dyn Fn() -> Result<(), String> + Sync),
    /// Trial 10 progress as JSON (`desk::Progress`).
    pub desk: &'a (dyn Fn() -> Result<String, String> + Sync),
    /// Account equity and SPY, both scaled to 100, as JSON.
    pub equity: &'a (dyn Fn() -> Result<String, String> + Sync),
    /// `data/lessons.jsonl` as a JSON array (DESIGN §13d).
    pub lessons: &'a (dyn Fn() -> Result<String, String> + Sync),
    /// The Quant Desk page's cached reading as JSON (`desk_reading`, S47).
    pub quant: &'a (dyn Fn() -> Result<String, String> + Sync),
    /// The key table as JSON (S43), cached with the desk reading.
    pub keys: &'a (dyn Fn() -> Result<String, String> + Sync),
    /// Do one of `TARGETS`' actions; Ok is what to tell the user.
    pub service: &'a (dyn Fn(&str, &str) -> Result<String, String> + Sync),
}

/// Fails reads past `until`, so a client dribbling bytes can't hold a thread for long.
pub struct Deadline<R> {
    pub r: R,
    pub until: Instant,
}

impl<R: Read> Read for Deadline<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.until {
            return Err(ErrorKind::TimedOut.into());
        }
        self.r.read(b)
    }
}

fn read_error(e: &std::io::Error) -> u16 {
    match e.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => 408,
        _ => 400,
    }
}

/// Reads one request. Errors are the status to answer with.
pub fn parse(r: &mut impl Read) -> Result<Request, u16> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(431);
        }
        match r.read(&mut chunk) {
            Ok(0) => return Err(400),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(read_error(&e)),
        }
    };
    if end > MAX_HEAD {
        return Err(431);
    }
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let start: Vec<&str> = lines.next().unwrap_or("").split(' ').collect();
    let [method, path, version] = start[..] else {
        return Err(400);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(400);
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for l in lines {
        let (n, v) = l.split_once(':').ok_or(400u16)?;
        // strict names: no "Host :" and no folded continuation lines
        if n.is_empty() || n.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(400);
        }
        let n = n.to_ascii_lowercase();
        // one of each that matters, and no chunked bodies: no ambiguity about what was sent
        let single = ["host", "authorization", "content-length", "origin"];
        if single.contains(&n.as_str()) && headers.iter().any(|(m, _)| *m == n) {
            return Err(400);
        }
        if n == "transfer-encoding" {
            return Err(400);
        }
        headers.push((n, v.trim().to_string()));
    }
    let len = match headers.iter().find(|(n, _)| n == "content-length") {
        None => 0,
        Some((_, v)) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
            v.parse::<usize>().map_err(|_| 413u16)?
        }
        Some(_) => return Err(400),
    };
    if len > MAX_BODY {
        return Err(413);
    }
    let mut body = buf[end + 4..].to_vec();
    if body.len() > len {
        return Err(400);
    }
    let mut rest = vec![0u8; len - body.len()];
    r.read_exact(&mut rest).map_err(|e| match e.kind() {
        ErrorKind::UnexpectedEof => 400,
        _ => read_error(&e),
    })?;
    body.extend_from_slice(&rest);
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body,
    })
}

/// Same time whatever the input, so the token can't be guessed byte by byte from timings.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

pub fn route(req: &Request, ctx: &Ctx) -> Response {
    // An exact Host blocks DNS rebinding: a page on evil.example resolving to 127.0.0.1.
    let hosts = [
        format!("127.0.0.1:{}", ctx.port),
        format!("localhost:{}", ctx.port),
    ];
    let Some(host) = req.header("host").filter(|h| hosts.iter().any(|x| x == h)) else {
        return Response::json(403, false, "wrong Host");
    };
    let known = ROUTES.iter().any(|(_, p)| *p == req.path);
    if !ROUTES.contains(&(req.method.as_str(), req.path.as_str())) {
        return if known {
            Response::json(405, false, "method not allowed")
        } else {
            Response::json(404, false, "not found")
        };
    }
    match req.path.as_str() {
        "/" => return Response::ok("text/html; charset=utf-8", INDEX),
        "/help" => return Response::ok("text/html; charset=utf-8", HELP),
        "/services" => return Response::ok("text/html; charset=utf-8", SERVICES),
        "/app.js" => return Response::ok("text/javascript; charset=utf-8", APP_JS),
        "/services.js" => return Response::ok("text/javascript; charset=utf-8", SERVICES_JS),
        "/desk" => return Response::ok("text/html; charset=utf-8", DESK),
        "/desk.js" => return Response::ok("text/javascript; charset=utf-8", DESK_JS),
        "/app.css" => return Response::ok("text/css; charset=utf-8", APP_CSS),
        "/chart.js" => return Response::ok("text/javascript; charset=utf-8", CHART_JS),
        _ => {}
    }
    let bearer = req
        .header("authorization")
        .and_then(|a| a.strip_prefix("Bearer "));
    if !bearer.is_some_and(|t| same(t, &ctx.token)) {
        return Response::json(
            401,
            false,
            "missing or wrong token: open the URL `ui` printed",
        );
    }
    match req.path.as_str() {
        "/api/state" | "/api/services" | "/api/desk" | "/api/equity" | "/api/lessons"
        | "/api/desk/snapshot" | "/api/keys" => {
            let read = match req.path.as_str() {
                "/api/state" => ctx.state,
                "/api/services" => ctx.services,
                "/api/desk" => ctx.desk,
                "/api/lessons" => ctx.lessons,
                "/api/desk/snapshot" => ctx.quant,
                "/api/keys" => ctx.keys,
                _ => ctx.equity,
            };
            match read() {
                Ok(json) => Response::ok("application/json", &json),
                Err(e) => Response::json(503, false, &e),
            }
        }
        "/api/stop" => control(req, ctx, host, "STOP"),
        "/api/run" => run_job(req, ctx, host),
        _ => control(req, ctx, host, "FLATTEN"),
    }
}

/// What every POST must carry: a JSON body and, when the browser sends one, our own Origin.
fn post_checks(req: &Request, host: &str) -> Option<Response> {
    let json = req
        .header("content-type")
        .is_some_and(|c| c.starts_with("application/json"));
    if !json {
        return Some(Response::json(415, false, "send application/json"));
    }
    if req
        .header("origin")
        .is_some_and(|o| o != format!("http://{host}"))
    {
        return Some(Response::json(403, false, "wrong Origin"));
    }
    None
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Act {
    target: String,
    action: String,
    confirm: String,
}

fn run_job(req: &Request, ctx: &Ctx, host: &str) -> Response {
    if let Some(r) = post_checks(req, host) {
        return r;
    }
    let Ok(r) = serde_json::from_slice::<Act>(&req.body) else {
        return Response::json(
            400,
            false,
            "send {\"target\": ..., \"action\": ..., \"confirm\": ...}",
        );
    };
    // target and action are taken from our own list, never from the request's text
    let Some((target, actions)) = TARGETS.iter().find(|(t, _)| *t == r.target) else {
        return Response::json(400, false, "unknown target");
    };
    let Some(action) = actions.iter().find(|a| **a == r.action) else {
        return Response::json(400, false, "that action isn't offered for this target");
    };
    if r.confirm != word(action) {
        return Response::json(400, false, &format!("type {} to confirm", word(action)));
    }
    let now = chrono::Utc::now().timestamp();
    eprintln!(
        "{} ET  {action} {target}, from the web console",
        watch::et(now)
    );
    let result = match (ctx.service)(target, action) {
        Ok(m) => Response::json(200, true, &m),
        Err(e) => Response::json(502, false, &e),
    };
    eprintln!("  -> {}", result.body);
    result
}

/// Does one allowlisted action. launchd: `kickstart` (a running job is left alone) or
/// `kickstart -k` (restart). OmniRoute: its own CLI, from ~/.omniroute with a cleared environment.
pub fn service(target: &str, action: &str) -> Result<String, String> {
    if target == "omniroute" {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set")?;
        if action == "restart" {
            // stopping one that isn't running is fine: serve brings it up either way
            let _ = crate::llm::omniroute_control(&home, &["stop"]);
        }
        crate::llm::omniroute_control(&home, &["serve", "--daemon"])?;
        return Ok(
            "OmniRoute is starting in the background; its row turns green within 30 s.".into(),
        );
    }
    let label = if target == "ollama" {
        "sh.brew.ollama".to_string()
    } else {
        format!("com.bmv-trader.{target}")
    };
    let uid = services::uid().ok_or("couldn't read the user id")?;
    let domain = format!("gui/{uid}/{label}");
    let args: Vec<&str> = if action == "restart" {
        vec!["kickstart", "-k", &domain]
    } else {
        vec!["kickstart", &domain]
    };
    let (code, _) =
        services::run("/bin/launchctl", &args, &[], false).ok_or("launchctl didn't answer")?;
    if code == 0 {
        Ok(format!(
            "{target}: {} sent. Its row shows the result within 30 s.",
            if action == "restart" {
                "restart"
            } else {
                "start"
            }
        ))
    } else {
        Err(format!(
            "launchctl {} failed (exit {code}); is {label} installed?",
            args.join(" ")
        ))
    }
}

/// Trial 10's progress; the calendar comes from the live SPY bars, as for `desk-report`.
fn desk_json(home: &Path) -> Result<String, String> {
    let dir = home.join("data");
    let spy = scan::read_daily(&dir, scan::ADJUSTED_SIP_LIVE, "SPY")?;
    let cal = momentum::Calendar::new(&spy)?;
    let today = watch::today_et(chrono::Utc::now().timestamp());
    let p = desk::progress(home, &cal, &today)?;
    serde_json::to_string(&p).map_err(|e| format!("desk: {e}"))
}

/// The desk page's cache after one reading (the desk's, or the key table's): `checked` is when it
/// was tried; `as_of` and `data` are the last reading that worked, so a source that's down shows
/// its last known state and why.
fn desk_reading(
    prev: Option<&serde_json::Value>,
    got: Result<serde_json::Value, String>,
    now: &str,
) -> serde_json::Value {
    match got {
        Ok(data) => serde_json::json!({"checked": now, "as_of": now, "error": null, "data": data}),
        Err(e) => serde_json::json!({
            "checked": now,
            "as_of": prev.map_or(serde_json::Value::Null, |p| p["as_of"].clone()),
            "error": e,
            "data": prev.map_or(serde_json::Value::Null, |p| p["data"].clone()),
        }),
    }
}

/// The lessons file as one JSON array; small, so it's read per request.
fn lessons_json(home: &Path) -> Result<String, String> {
    let path = home.join("data").join("lessons.jsonl");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "no lessons yet: run `bmv-trader trial-autopsy`".to_string())?;
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("lessons.jsonl: {e}"))?;
    serde_json::to_string(&rows).map_err(|e| e.to_string())
}

#[derive(serde::Deserialize)]
struct History {
    timestamp: Vec<i64>,
    equity: Vec<Option<f64>>,
}

#[derive(serde::Serialize, Debug, PartialEq)]
struct Point {
    time: String,
    value: f64,
}

/// Both series from the first date they share, each scaled to 100 there.
fn scaled(equity: &[(String, f64)], spy: &[(String, f64)]) -> (Vec<Point>, Vec<Point>) {
    let spy_on = |d: &str| spy.iter().find(|(s, _)| s == d).map(|(_, c)| *c);
    let Some((start, e0, s0)) = equity
        .iter()
        .find_map(|(d, e)| spy_on(d).map(|s| (d.clone(), *e, s)))
    else {
        return (Vec::new(), Vec::new());
    };
    let from = |xs: &[(String, f64)], base: f64| {
        xs.iter()
            .filter(|(d, _)| *d >= start)
            .map(|(d, v)| Point {
                time: d.clone(),
                value: v / base * 100.0,
            })
            .collect()
    };
    (from(equity, e0), from(spy, s0))
}

fn equity_json(a: &Alpaca, home: &Path) -> Result<String, String> {
    let h: History = a.get(
        Api::Trading,
        "/v2/account/portfolio/history?period=1A&timeframe=1D",
    )?;
    // days before the account was funded come back as 0 or null
    let equity: Vec<(String, f64)> = h
        .timestamp
        .iter()
        .zip(&h.equity)
        .filter_map(|(t, e)| e.filter(|e| *e > 0.0).map(|e| (watch::today_et(*t), e)))
        .collect();
    let spy: Vec<(String, f64)> =
        scan::read_daily(&home.join("data"), scan::ADJUSTED_SIP_LIVE, "SPY")?
            .into_iter()
            .map(|b| (b.date, b.close))
            .collect();
    let (equity, spy) = scaled(&equity, &spy);
    Ok(serde_json::json!({ "equity": equity, "spy": spy }).to_string())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    confirm: String,
}

fn control(req: &Request, ctx: &Ctx, host: &str, word: &str) -> Response {
    if let Some(r) = post_checks(req, host) {
        return r;
    }
    match serde_json::from_slice::<Confirm>(&req.body) {
        Ok(c) if c.confirm == word => {}
        _ => return Response::json(400, false, &format!("type {word} to confirm")),
    }
    let now = chrono::Utc::now().timestamp();
    let reason = format!("ui {} ET, {word} from the web console", watch::et(now));
    eprintln!(
        "{} ET  {word} requested from the web console",
        watch::et(now)
    );
    // Flatten engages STOP first too, so the engine takes no new entry while it closes.
    let engaged = safety::engage(&ctx.stop, &reason);
    let stop = match &engaged {
        Ok(Engaged::Now) => "STOP engaged".to_string(),
        Ok(Engaged::Already) => "STOP was already engaged".to_string(),
        Ok(Engaged::NowWithoutReason(e)) => format!("STOP engaged (reason not saved: {e})"),
        Err(e) => format!("STOP FAILED: {e}"),
    };
    eprintln!("  {stop}");
    let result = if word == "STOP" {
        match engaged {
            Ok(_) => Response::json(
                200,
                true,
                &format!(
                    "{stop}. A running engine stops entering and cancels its working entries on \
                     its next tick. Close everything cancels at Alpaca now."
                ),
            ),
            Err(_) => Response::json(500, false, &stop),
        }
    } else {
        // Flatten even if STOP couldn't be written: getting flat matters more.
        match ((ctx.flatten)(), engaged.is_ok()) {
            (Ok(()), true) => Response::json(
                200,
                true,
                &format!("Flat: no positions or open orders left. {stop}; it stays engaged."),
            ),
            (Ok(()), false) => Response::json(
                500,
                false,
                &format!("Flat, but {stop}. Create data/STOP by hand."),
            ),
            (Err(e), _) => Response::json(502, false, &format!("FLATTEN FAILED: {e}. {stop}.")),
        }
    };
    eprintln!("  -> {}", result.body);
    (ctx.refresh)();
    result
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        408 => "Request Timeout",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Bad Gateway",
    }
}

pub fn write(w: &mut impl Write, r: &Response) -> std::io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; \
         connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCache-Control: no-store\r\n\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len()
    )?;
    w.write_all(r.body.as_bytes())?;
    w.flush()
}

/// 32 random bytes as hex, fresh per launch.
fn token() -> Result<String, String> {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("no randomness for the console token: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn handle(stream: TcpStream, ctx: &Ctx) {
    // Each read waits at most 1 s, and the whole request at most REQUEST_SECS.
    if stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .and(stream.set_write_timeout(Some(Duration::from_secs(5))))
        .is_err()
    {
        return; // never serve without timeouts
    }
    let until = Instant::now() + Duration::from_secs(REQUEST_SECS);
    let mut d = Deadline { r: &stream, until };
    let resp = match parse(&mut d) {
        Ok(req) => route(&req, ctx),
        Err(code) => Response::json(code, false, reason(code)),
    };
    if let Err(e) = write(&mut &stream, &resp) {
        eprintln!("reply: {e}");
    }
}

/// Serves until killed. Alpaca is read on its own thread and each connection gets a thread, so a
/// slow client or a slow Alpaca never delays STOP.
pub fn serve(
    a: &Alpaca,
    home: &Path,
    mode: &str,
    port: u16,
    keys: &(dyn Fn() -> Result<String, String> + Sync),
    open: bool,
) -> Result<(), String> {
    let listener =
        TcpListener::bind((ADDR, port)).map_err(|e| format!("listen on {ADDR}:{port}: {e}"))?;
    let token = token()?;
    let latest: Mutex<Option<Result<String, String>>> = Mutex::new(None);
    let dirty = AtomicBool::new(false);
    let flattening = Mutex::new(());
    let state = || {
        let l = latest.lock().unwrap_or_else(|p| p.into_inner());
        l.clone()
            .unwrap_or_else(|| Err("first reading not ready yet".to_string()))
    };
    let checked: Mutex<Option<Result<String, String>>> = Mutex::new(None);
    let services = || {
        let l = checked.lock().unwrap_or_else(|p| p.into_inner());
        l.clone()
            .unwrap_or_else(|| Err("first check still running".to_string()))
    };
    let desk_latest: Mutex<Option<Result<String, String>>> = Mutex::new(None);
    let equity_latest: Mutex<Option<Result<String, String>>> = Mutex::new(None);
    let read = |m: &Mutex<Option<Result<String, String>>>| {
        let l = m.lock().unwrap_or_else(|p| p.into_inner());
        l.clone()
            .unwrap_or_else(|| Err("first reading not ready yet".to_string()))
    };
    let desk = || read(&desk_latest);
    let quant_latest: Mutex<Option<serde_json::Value>> = Mutex::new(None);
    let keys_latest: Mutex<Option<serde_json::Value>> = Mutex::new(None);
    let cached = |m: &Mutex<Option<serde_json::Value>>| {
        let l = m.lock().unwrap_or_else(|p| p.into_inner());
        l.as_ref()
            .map(|v| v.to_string())
            .ok_or_else(|| "first reading not ready yet".to_string())
    };
    let quant = || cached(&quant_latest);
    let keys_read = || cached(&keys_latest);
    let lessons = || lessons_json(home);
    let equity = || read(&equity_latest);
    let refresh = || dirty.store(true, Ordering::SeqCst);
    let flatten = || {
        let _one = flattening
            .try_lock()
            .map_err(|_| "a flatten is already running".to_string())?;
        live::flatten(a, 10, &mut || std::thread::sleep(Duration::from_secs(1)))
    };
    let ctx = Ctx {
        port,
        token,
        stop: safety::stop_path(home),
        state: &state,
        services: &services,
        refresh: &refresh,
        flatten: &flatten,
        desk: &desk,
        equity: &equity,
        lessons: &lessons,
        quant: &quant,
        keys: &keys_read,
        service: &service,
    };
    println!(
        "web console ({mode}), this Mac only. Open:\n\n  http://{ADDR}:{port}/#{}\n",
        ctx.token
    );
    println!("Keep this URL private: it is the key to STOP and flatten. Ctrl-C quits.");
    if open {
        // the token travels as an argument to `open`, on this Mac only, as when you click the URL
        let url = format!("http://{ADDR}:{port}/#{}", ctx.token);
        if let Err(e) = std::process::Command::new("/usr/bin/open")
            .arg(&url)
            .status()
        {
            eprintln!("couldn't open the browser ({e}); open the URL above");
        }
    }
    let active = AtomicUsize::new(0);
    std::thread::scope(|s| {
        s.spawn(|| {
            loop {
                dirty.store(false, Ordering::SeqCst);
                let json = serde_json::to_string(&watch::snapshot(a, home, mode, 50))
                    .map_err(|e| format!("snapshot: {e}"));
                *latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(json);
                for _ in 0..CACHE_SECS * 4 {
                    if dirty.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        });
        // its own thread: a slow Keychain or launchctl never delays the account reading
        s.spawn(|| {
            loop {
                let json = serde_json::to_string(&services::check(home, port))
                    .map_err(|e| format!("services: {e}"));
                *checked.lock().unwrap_or_else(|p| p.into_inner()) = Some(json);
                std::thread::sleep(Duration::from_secs(SERVICES_SECS));
            }
        });
        // reads every rated signal's bars and Alpaca's history: slow, and needed rarely
        s.spawn(|| {
            loop {
                *desk_latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(desk_json(home));
                *equity_latest.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(equity_json(a, home));
                std::thread::sleep(Duration::from_secs(RESEARCH_SECS));
            }
        });
        // the Quant Desk page (S47): one reading per DESK_SECS, kept when the desk is down
        s.spawn(|| {
            // tried again every reading until it works, so adding the token needs no restart
            let mut reader = None;
            loop {
                let got = match reader
                    .take()
                    .map_or_else(crate::desk_link::Reader::from_keychain, Ok)
                {
                    Ok(r) => {
                        let got = r.snapshot();
                        // a failed reading logs in again next time: a reissued token needs no restart
                        if got.is_ok() {
                            reader = Some(r);
                        }
                        got
                    }
                    Err(e) => Err(e),
                };
                let now = chrono::Utc::now().to_rfc3339();
                let mut l = quant_latest.lock().unwrap_or_else(|p| p.into_inner());
                *l = Some(desk_reading(l.as_ref(), got, &now));
                drop(l);
                let table = keys().and_then(|s| {
                    serde_json::from_str(&s).map_err(|e| format!("the key table: {e}"))
                });
                let mut l = keys_latest.lock().unwrap_or_else(|p| p.into_inner());
                *l = Some(desk_reading(l.as_ref(), table, &now));
                drop(l);
                std::thread::sleep(Duration::from_secs(DESK_SECS));
            }
        });
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("accept: {e}");
                    continue;
                }
            };
            // shortcut: a fixed cap on connection threads; extra connections are just closed.
            if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                active.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let (ctx, active) = (&ctx, &active);
            s.spawn(move || {
                handle(stream, ctx);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    const TOKEN: &str = "abc123";

    fn tmp() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "bmv-ui-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let mut h: Vec<(String, String)> = vec![("host".into(), "127.0.0.1:8765".into())];
        for (n, v) in headers {
            h.retain(|(m, _)| m != n);
            h.push((n.to_string(), v.to_string()));
        }
        Request {
            method: method.into(),
            path: path.into(),
            headers: h,
            body: body.as_bytes().to_vec(),
        }
    }

    const AUTH: (&str, &str) = ("authorization", "Bearer abc123");
    const JSON: (&str, &str) = ("content-type", "application/json");

    /// Runs `route` with fakes; returns the response, whether STOP exists after, and the calls.
    /// `prepare` sets up the temp dir; `flat` is what the fake flatten answers.
    fn call_with(
        r: &Request,
        prepare: fn(&Path),
        flat: Result<(), String>,
    ) -> (Response, bool, Vec<String>) {
        let dir = tmp();
        prepare(&dir);
        let stop = dir.join("STOP");
        let calls = Mutex::new(Vec::new());
        let log = |c: String| calls.lock().unwrap().push(c);
        let state = || {
            log("state".into());
            Ok("{}".to_string())
        };
        let services = || {
            log("services".into());
            Ok(r#"{"now":1,"components":[]}"#.to_string())
        };
        let refresh = || log("refresh".into());
        let flatten = || {
            // STOP must already be engaged when the broker is asked to close
            log(format!("flatten stop={}", stop.exists()));
            flat.clone()
        };
        let desk = || {
            log("desk".into());
            Ok("{}".to_string())
        };
        let equity = || {
            log("equity".into());
            Ok("{}".to_string())
        };
        let lessons = || {
            log("lessons".into());
            Ok("[]".to_string())
        };
        let service = |target: &str, action: &str| {
            log(format!("{action} {target}"));
            Ok(format!("{target} {action}"))
        };
        let ctx = Ctx {
            port: 8765,
            token: TOKEN.into(),
            stop: stop.clone(),
            state: &state,
            services: &services,
            refresh: &refresh,
            flatten: &flatten,
            desk: &desk,
            equity: &equity,
            lessons: &lessons,
            quant: &desk,
            keys: &lessons,
            service: &service,
        };
        let resp = route(r, &ctx);
        let engaged = stop.exists();
        std::fs::remove_dir_all(&dir).unwrap();
        (resp, engaged, calls.into_inner().unwrap())
    }

    fn call(r: &Request) -> (Response, bool, Vec<String>) {
        call_with(r, |_| {}, Ok(()))
    }

    #[test]
    fn the_route_table_is_exactly_monitor_stop_flatten_run() {
        let paths: Vec<_> = ROUTES.iter().map(|(m, p)| format!("{m} {p}")).collect();
        assert_eq!(
            paths,
            [
                "GET /",
                "GET /help",
                "GET /services",
                "GET /desk",
                "GET /app.js",
                "GET /services.js",
                "GET /desk.js",
                "GET /app.css",
                "GET /chart.js",
                "GET /api/state",
                "GET /api/services",
                "GET /api/desk",
                "GET /api/equity",
                "GET /api/lessons",
                "GET /api/desk/snapshot",
                "GET /api/keys",
                "POST /api/stop",
                "POST /api/flatten",
                "POST /api/run"
            ]
        );
    }

    #[test]
    fn the_desk_page_has_no_controls_and_its_data_needs_the_token() {
        let page = call(&req("GET", "/desk", &[], "")).0;
        assert_eq!(page.status, 200);
        assert!(
            !page.body.contains("<form"),
            "no form: approvals stay in desk sign"
        );
        assert!(!page.body.contains("style=") && !page.body.contains("<script>"));
        let js = call(&req("GET", "/desk.js", &[], "")).0;
        assert_eq!(js.status, 200);
        assert!(!js.body.contains("innerHTML") && !js.body.contains("insertAdjacentHTML"));
        assert!(!js.body.contains("method:"), "every fetch is a GET");
        let fetched: Vec<_> = js
            .body
            .match_indices("get(\"/api/")
            .map(|(i, _)| js.body[i + 5..].split('"').next().unwrap())
            .collect();
        assert_eq!(fetched, ["/api/desk/snapshot", "/api/keys"]);
        for path in ["/api/desk/snapshot", "/api/keys"] {
            assert_eq!(call(&req("GET", path, &[], "")).0.status, 401);
            assert_eq!(call(&req("POST", path, &[AUTH], "")).0.status, 405);
        }
        let (resp, _, calls) = call(&req("GET", "/api/desk/snapshot", &[AUTH], ""));
        assert_eq!((resp.status, calls), (200, vec!["desk".to_string()]));
        for page in ["/", "/help", "/services"] {
            let body = call(&req("GET", page, &[], "")).0.body;
            assert!(body.contains("href=\"/desk\""), "{page} links to Desk");
        }
    }

    #[test]
    fn a_desk_that_is_down_keeps_its_last_reading_and_says_why() {
        let first = desk_reading(None, Err("no console token".into()), "t0");
        assert_eq!(first["data"], serde_json::Value::Null);
        assert_eq!(first["error"], "no console token");
        let good = desk_reading(
            Some(&first),
            Ok(serde_json::json!({"strategies": []})),
            "t1",
        );
        assert_eq!(
            (good["as_of"].as_str(), good["error"].is_null()),
            (Some("t1"), true)
        );
        let down = desk_reading(Some(&good), Err("can't reach the desk".into()), "t2");
        assert_eq!(down["checked"], "t2");
        assert_eq!(down["as_of"], "t1", "the last reading that worked");
        assert_eq!(down["data"]["strategies"], serde_json::json!([]));
        assert_eq!(down["error"], "can't reach the desk");
    }

    #[test]
    fn the_services_page_has_one_control_and_its_data_needs_the_token() {
        let page = call(&req("GET", "/services", &[], "")).0;
        assert_eq!(page.status, 200);
        assert_eq!(page.body.matches("<form").count(), 1, "only Run now");
        assert!(!page.body.contains("style=") && !page.body.contains("<script>"));
        let js = call(&req("GET", "/services.js", &[], "")).0;
        assert_eq!(js.status, 200);
        assert!(!js.body.contains("innerHTML"));
        let posts: Vec<_> = js
            .body
            .match_indices("fetch(\"")
            .map(|(i, _)| &js.body[i..i + 18])
            .collect();
        assert_eq!(posts, ["fetch(\"/api/servic", "fetch(\"/api/run\", "]);
        assert_eq!(call(&req("GET", "/api/services", &[], "")).0.status, 401);
        let bad = ("authorization", "Bearer nope");
        assert_eq!(call(&req("GET", "/api/services", &[bad], "")).0.status, 401);
        let (resp, _, calls) = call(&req("GET", "/api/services", &[AUTH], ""));
        assert_eq!((resp.status, calls), (200, vec!["services".to_string()]));
        assert_eq!(resp.content_type, "application/json");
        assert_eq!(
            call(&req("POST", "/api/services", &[AUTH], "")).0.status,
            405
        );
        for page in ["/", "/help"] {
            let body = call(&req("GET", page, &[], "")).0.body;
            assert!(
                body.contains("href=\"/services\""),
                "{page} links to Services"
            );
        }
    }

    #[test]
    fn static_files_need_only_the_right_host() {
        assert_eq!(call(&req("GET", "/", &[], "")).0.status, 200);
        assert_eq!(call(&req("GET", "/app.js", &[], "")).0.status, 200);
        let help = call(&req("GET", "/help", &[], "")).0;
        assert_eq!(help.status, 200);
        assert!(
            !help.body.contains("<script"),
            "the how-to page runs no code"
        );
        let other = [("host", "localhost:8765")];
        assert_eq!(call(&req("GET", "/app.css", &other, "")).0.status, 200);
    }

    #[test]
    fn a_foreign_host_is_refused_everywhere() {
        for host in ["evil.example:8765", "127.0.0.1:9999", "127.0.0.1"] {
            let r = req("GET", "/", &[("host", host)], "");
            assert_eq!(call(&r).0.status, 403, "{host}");
        }
        let mut no_host = req("GET", "/", &[], "");
        no_host.headers.clear();
        assert_eq!(call(&no_host).0.status, 403);
    }

    #[test]
    fn the_api_needs_the_exact_token() {
        assert_eq!(call(&req("GET", "/api/state", &[], "")).0.status, 401);
        for bad in ["Bearer abc12", "Bearer abc1234", "abc123", "Bearer ABC123"] {
            let r = req("GET", "/api/state", &[("authorization", bad)], "");
            assert_eq!(call(&r).0.status, 401, "{bad}");
        }
        let (resp, _, calls) = call(&req("GET", "/api/state", &[AUTH], ""));
        assert_eq!((resp.status, calls), (200, vec!["state".to_string()]));
    }

    #[test]
    fn unknown_paths_and_wrong_methods() {
        assert_eq!(call(&req("GET", "/api/clear", &[AUTH], "")).0.status, 404);
        assert_eq!(call(&req("GET", "/api/stop", &[AUTH], "")).0.status, 405);
        assert_eq!(call(&req("DELETE", "/", &[], "")).0.status, 405);
        assert_eq!(call(&req("POST", "/api/state", &[AUTH], "")).0.status, 405);
    }

    #[test]
    fn stop_needs_json_the_word_and_a_same_origin() {
        let ok = r#"{"confirm":"STOP"}"#;
        let (resp, engaged, _) = call(&req("POST", "/api/stop", &[AUTH], ok));
        assert_eq!((resp.status, engaged), (415, false));
        for body in [
            r#"{"confirm":"stop"}"#,
            r#"{"confirm":"STOP","x":1}"#,
            "",
            "{",
        ] {
            let (resp, engaged, _) = call(&req("POST", "/api/stop", &[AUTH, JSON], body));
            assert_eq!((resp.status, engaged), (400, false), "{body}");
        }
        let evil = ("origin", "http://evil.example");
        let (resp, engaged, _) = call(&req("POST", "/api/stop", &[AUTH, JSON, evil], ok));
        assert_eq!((resp.status, engaged), (403, false));
        let same = ("origin", "http://127.0.0.1:8765");
        let (resp, engaged, calls) = call(&req("POST", "/api/stop", &[AUTH, JSON, same], ok));
        assert_eq!((resp.status, engaged), (200, true));
        assert_eq!(calls, ["refresh"], "STOP never flattens");
        assert!(
            resp.body.contains("next tick"),
            "no promise of a cancel it didn't do"
        );
        let (resp, engaged, _) = call(&req("POST", "/api/stop", &[JSON], ok));
        assert_eq!((resp.status, engaged), (401, false));
    }

    #[test]
    fn flatten_engages_stop_before_closing() {
        let body = r#"{"confirm":"FLATTEN"}"#;
        let (resp, engaged, calls) = call(&req("POST", "/api/flatten", &[AUTH, JSON], body));
        assert_eq!((resp.status, engaged), (200, true));
        assert_eq!(calls, ["flatten stop=true", "refresh"]);
        let wrong = r#"{"confirm":"STOP"}"#;
        let (resp, engaged, calls) = call(&req("POST", "/api/flatten", &[AUTH, JSON], wrong));
        assert_eq!((resp.status, engaged, calls.len()), (400, false, 0));
    }

    #[test]
    fn stop_reports_already_engaged_and_flatten_runs_even_when_stop_cannot_be_written() {
        let stop = r#"{"confirm":"STOP"}"#;
        let flat = r#"{"confirm":"FLATTEN"}"#;
        let present = |d: &Path| std::fs::write(d.join("STOP"), "earlier").unwrap();
        let (resp, engaged, _) = call_with(
            &req("POST", "/api/stop", &[AUTH, JSON], stop),
            present,
            Ok(()),
        );
        assert_eq!((resp.status, engaged), (200, true));
        assert!(resp.body.contains("already engaged"), "{}", resp.body);
        // STOP's parent is a file, so STOP can't be created at all
        let blocked = |d: &Path| {
            std::fs::remove_dir_all(d).unwrap();
            std::fs::write(d, "").unwrap();
        };
        let (resp, _, calls) =
            call_with_file(&req("POST", "/api/stop", &[AUTH, JSON], stop), blocked);
        assert_eq!(resp.status, 500);
        assert!(calls.is_empty() || calls == ["refresh"]);
        let (resp, _, calls) =
            call_with_file(&req("POST", "/api/flatten", &[AUTH, JSON], flat), blocked);
        assert_eq!(resp.status, 500, "{}", resp.body);
        assert!(resp.body.contains("Flat, but STOP FAILED"), "{}", resp.body);
        assert_eq!(calls, ["flatten stop=false", "refresh"]);
        let failed = Err("positions left: SPY".to_string());
        let (resp, engaged, _) = call_with(
            &req("POST", "/api/flatten", &[AUTH, JSON], flat),
            |_| {},
            failed,
        );
        assert_eq!((resp.status, engaged), (502, true));
        assert!(resp.body.contains("FLATTEN FAILED: positions left: SPY"));
    }

    /// Like `call_with`, for a temp "dir" that `prepare` turns into a file.
    fn call_with_file(r: &Request, prepare: fn(&Path)) -> (Response, bool, Vec<String>) {
        let dir = tmp();
        prepare(&dir);
        let calls = Mutex::new(Vec::new());
        let log = |c: String| calls.lock().unwrap().push(c);
        let stop = dir.join("STOP");
        let state = || Ok(String::new());
        let refresh = || log("refresh".into());
        let flatten = || {
            log(format!("flatten stop={}", stop.exists()));
            Ok(())
        };
        let ctx = Ctx {
            port: 8765,
            token: TOKEN.into(),
            stop: stop.clone(),
            state: &state,
            services: &state,
            refresh: &refresh,
            flatten: &flatten,
            desk: &state,
            equity: &state,
            lessons: &state,
            quant: &state,
            keys: &state,
            service: &|_, _| Err("not in this test".into()),
        };
        let resp = route(r, &ctx);
        std::fs::remove_file(&dir).unwrap();
        (resp, false, calls.into_inner().unwrap())
    }

    #[test]
    fn state_not_ready_is_503() {
        let state = || Err("first reading not ready yet".to_string());
        let (refresh, flatten) = (|| {}, || Ok(()));
        let ctx = Ctx {
            port: 8765,
            token: TOKEN.into(),
            stop: PathBuf::from("/nonexistent/STOP"),
            state: &state,
            services: &state,
            refresh: &refresh,
            flatten: &flatten,
            desk: &state,
            equity: &state,
            lessons: &state,
            quant: &state,
            keys: &state,
            service: &|_, _| Err("not in this test".into()),
        };
        for path in [
            "/api/state",
            "/api/services",
            "/api/desk",
            "/api/equity",
            "/api/lessons",
            "/api/desk/snapshot",
            "/api/keys",
        ] {
            assert_eq!(route(&req("GET", path, &[AUTH], ""), &ctx).status, 503);
        }
    }

    #[test]
    fn every_response_carries_the_security_headers() {
        let mut out = Vec::new();
        write(&mut out, &Response::json(404, false, "x")).unwrap();
        let text = String::from_utf8(out).unwrap();
        for h in [
            "Content-Security-Policy: default-src 'none'",
            "frame-ancestors 'none'",
            "X-Content-Type-Options: nosniff",
            "Cache-Control: no-store",
            "Content-Length: ",
        ] {
            assert!(text.contains(h), "{h}");
        }
        assert!(!text.contains("Access-Control"), "no CORS, ever");
    }

    fn parse_str(s: &str) -> Result<Request, u16> {
        parse(&mut s.as_bytes())
    }

    #[test]
    fn the_parser_reads_a_request_and_caps_everything() {
        let r = parse_str(
            "POST /api/stop HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nContent-Length: 4\r\n\r\nabcd",
        )
        .unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/api/stop"));
        assert_eq!(
            (r.header("host"), r.body.as_slice()),
            (Some("127.0.0.1:8765"), &b"abcd"[..])
        );
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(parse_str(&huge).unwrap_err(), 431);
        let big = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(parse_str(&big).unwrap_err(), 413);
        for bad in [
            "GET /\r\n\r\n",
            "GET / SPDY/3\r\n\r\n",
            "GET / HTTP/1.1\r\nno colon\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\nshort",
            "POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\ntoo long",
            "GET / HTTP/1.1\r\n",
        ] {
            assert_eq!(parse_str(bad).unwrap_err(), 400, "{bad:?}");
        }
    }

    /// Hands out one byte per read, then fails with `end` once the data runs out.
    struct Drip<'a>(&'a [u8], ErrorKind);
    impl Read for Drip<'_> {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            match self.0.split_first() {
                Some((x, rest)) if !b.is_empty() => {
                    b[0] = *x;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Err(self.1.into()),
            }
        }
    }

    #[test]
    fn the_parser_handles_partial_reads_stalls_and_deadlines() {
        let full = b"POST /api/stop HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\n\r\nabcd";
        let r = parse(&mut Drip(full, ErrorKind::TimedOut)).unwrap();
        assert_eq!(r.body, b"abcd");
        // stalls mid-head or mid-body: 408, not a hang
        assert_eq!(
            parse(&mut Drip(&full[..20], ErrorKind::WouldBlock)).unwrap_err(),
            408
        );
        assert_eq!(
            parse(&mut Drip(&full[..full.len() - 2], ErrorKind::TimedOut)).unwrap_err(),
            408
        );
        // a head that never ends is cut off at the cap
        let endless = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(MAX_HEAD + 10));
        assert_eq!(parse(&mut endless.as_bytes()).unwrap_err(), 431);
        // past the deadline, nothing more is read
        let mut late = Deadline {
            r: &full[..],
            until: Instant::now(),
        };
        assert_eq!(parse(&mut late).unwrap_err(), 408);
        for bad in [
            "GET / HTTP/1.1\r\nHost : h\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: h\r\n folded\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: +4\r\n\r\nabcd",
        ] {
            assert_eq!(parse_str(bad).unwrap_err(), 400, "{bad:?}");
        }
    }

    #[test]
    fn tokens_are_long_hex_and_fresh() {
        let (a, b) = (token().unwrap(), token().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "abcd"));
    }

    #[test]
    fn service_actions_are_allowlisted_and_need_their_word() {
        let ok = r#"{"target":"notes","action":"run","confirm":"RUN"}"#;
        let (resp, engaged, calls) = call(&req("POST", "/api/run", &[AUTH, JSON], ok));
        assert_eq!((resp.status, engaged), (200, false), "{}", resp.body);
        assert_eq!(calls, ["run notes"]);
        for (body, want) in [
            (
                r#"{"target":"ollama","action":"restart","confirm":"RESTART"}"#,
                "restart ollama",
            ),
            (
                r#"{"target":"omniroute","action":"start","confirm":"START"}"#,
                "start omniroute",
            ),
            (
                r#"{"target":"desk","action":"restart","confirm":"RESTART"}"#,
                "restart desk",
            ),
        ] {
            let (resp, _, calls) = call(&req("POST", "/api/run", &[AUTH, JSON], body));
            assert_eq!(
                (resp.status, calls),
                (200, vec![want.to_string()]),
                "{body}"
            );
        }
        for bad in [
            r#"{"target":"notes","action":"run","confirm":"run"}"#,
            r#"{"target":"desk","action":"restart","confirm":"RUN"}"#,
            r#"{"target":"desk","action":"start","confirm":"START"}"#,
            r#"{"target":"ollama","action":"run","confirm":"RUN"}"#,
            r#"{"target":"run","action":"start","confirm":"START"}"#,
            r#"{"target":"engine","action":"start","confirm":"START"}"#,
            r#"{"target":"ollama; rm -rf /","action":"restart","confirm":"RESTART"}"#,
            r#"{"target":"notes","action":"run","confirm":"RUN","extra":1}"#,
            r#"{"job":"notes","confirm":"RUN"}"#,
        ] {
            let (resp, _, calls) = call(&req("POST", "/api/run", &[AUTH, JSON], bad));
            assert_eq!((resp.status, calls.len()), (400, 0), "{bad}");
        }
        assert_eq!(call(&req("POST", "/api/run", &[JSON], ok)).0.status, 401);
        assert_eq!(call(&req("POST", "/api/run", &[AUTH], ok)).0.status, 415);
        let evil = ("origin", "http://evil.example");
        let (resp, _, calls) = call(&req("POST", "/api/run", &[AUTH, JSON, evil], ok));
        assert_eq!((resp.status, calls.len()), (403, 0));
        assert_eq!(call(&req("GET", "/api/run", &[AUTH], "")).0.status, 405);
        let targets: Vec<&str> = TARGETS.iter().map(|t| t.0).collect();
        assert_eq!(
            targets,
            ["ollama", "omniroute", "desk", "notes", "llm"],
            "no trading engine, nothing that can place an order"
        );
    }

    #[test]
    fn research_reads_need_the_token() {
        for path in ["/api/desk", "/api/equity", "/api/lessons"] {
            assert_eq!(call(&req("GET", path, &[], "")).0.status, 401);
            let (resp, _, calls) = call(&req("GET", path, &[AUTH], ""));
            assert_eq!((resp.status, calls), (200, vec![path[5..].to_string()]));
        }
    }

    #[test]
    fn the_chart_library_is_the_pinned_file() {
        // macOS's shasum: no hashing crate just for a test
        let out = std::process::Command::new("/usr/bin/shasum")
            .args(["-a", "256", "src/ui/vendor/lightweight-charts.js"])
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(CHART_JS.len(), 189_472);
        assert_eq!(
            text.split_whitespace().next().unwrap_or(""),
            "756b0ba9f58930226d6b8b9d47ad310448ab85d799ea1f7c2c02304777c10b52",
            "lightweight-charts 5.1.0 standalone; a change must be a reviewed upgrade"
        );
        let r = call(&req("GET", "/chart.js", &[], "")).0;
        assert_eq!(
            (r.status, r.content_type),
            (200, "text/javascript; charset=utf-8")
        );
    }

    #[test]
    fn equity_and_spy_start_at_100_on_their_first_shared_day() {
        let e = |d: &str, v: f64| (d.to_string(), v);
        let equity = [e("2026-10-01", 200.0), e("2026-10-02", 210.0)];
        let spy = [
            e("2026-09-30", 1.0),
            e("2026-10-01", 50.0),
            e("2026-10-02", 45.0),
        ];
        let (eq, sp) = scaled(&equity, &spy);
        let vals = |xs: &[Point]| {
            xs.iter()
                .map(|p| (p.time.clone(), p.value))
                .collect::<Vec<_>>()
        };
        assert_eq!(vals(&eq), [e("2026-10-01", 100.0), e("2026-10-02", 105.0)]);
        assert_eq!(vals(&sp), [e("2026-10-01", 100.0), e("2026-10-02", 90.0)]);
        assert_eq!(scaled(&equity, &[]), (vec![], vec![]));
    }
}
