//! Local web console (DECISION S16): the same view as `watch`, plus STOP and flatten. It can only
//! make things safer or show them; there is no route to clear STOP, start a run or place an order.
//! Listens on 127.0.0.1 only, behind a token minted per launch.

use crate::broker::Alpaca;
use crate::safety::{self, Engaged};
use crate::{live, services, watch};
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
/// The Services checks (Keychain, launchctl, loopback HTTP) run at most once per this.
const SERVICES_SECS: u64 = 30;

/// Every route there is. A test pins it, so a new control can't slip in unreviewed.
pub const ROUTES: [(&str, &str); 10] = [
    ("GET", "/"),
    ("GET", "/help"),
    ("GET", "/services"),
    ("GET", "/app.js"),
    ("GET", "/services.js"),
    ("GET", "/app.css"),
    ("GET", "/api/state"),
    ("GET", "/api/services"),
    ("POST", "/api/stop"),
    ("POST", "/api/flatten"),
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
        "/app.css" => return Response::ok("text/css; charset=utf-8", APP_CSS),
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
        "/api/state" | "/api/services" => {
            let read = if req.path == "/api/state" {
                ctx.state
            } else {
                ctx.services
            };
            match read() {
                Ok(json) => Response::ok("application/json", &json),
                Err(e) => Response::json(503, false, &e),
            }
        }
        "/api/stop" => control(req, ctx, host, "STOP"),
        _ => control(req, ctx, host, "FLATTEN"),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    confirm: String,
}

fn control(req: &Request, ctx: &Ctx, host: &str, word: &str) -> Response {
    let json = req
        .header("content-type")
        .is_some_and(|c| c.starts_with("application/json"));
    if !json {
        return Response::json(415, false, "send application/json");
    }
    if req
        .header("origin")
        .is_some_and(|o| o != format!("http://{host}"))
    {
        return Response::json(403, false, "wrong Origin");
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
pub fn serve(a: &Alpaca, home: &Path, mode: &str, port: u16) -> Result<(), String> {
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
    };
    println!(
        "web console ({mode}), this Mac only. Open:\n\n  http://{ADDR}:{port}/#{}\n",
        ctx.token
    );
    println!("Keep this URL private: it is the key to STOP and flatten. Ctrl-C quits.");
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
        let ctx = Ctx {
            port: 8765,
            token: TOKEN.into(),
            stop: stop.clone(),
            state: &state,
            services: &services,
            refresh: &refresh,
            flatten: &flatten,
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
    fn the_route_table_is_exactly_monitor_stop_flatten() {
        let paths: Vec<_> = ROUTES.iter().map(|(m, p)| format!("{m} {p}")).collect();
        assert_eq!(
            paths,
            [
                "GET /",
                "GET /help",
                "GET /services",
                "GET /app.js",
                "GET /services.js",
                "GET /app.css",
                "GET /api/state",
                "GET /api/services",
                "POST /api/stop",
                "POST /api/flatten"
            ]
        );
    }

    #[test]
    fn the_services_page_is_read_only_and_its_data_needs_the_token() {
        let page = call(&req("GET", "/services", &[], "")).0;
        assert_eq!(page.status, 200);
        assert!(!page.body.contains("<button") && !page.body.contains("<form"));
        assert!(!page.body.contains("style=") && !page.body.contains("<script>"));
        let js = call(&req("GET", "/services.js", &[], "")).0;
        assert_eq!(js.status, 200);
        assert!(!js.body.contains("innerHTML") && !js.body.contains("method:"));
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
        };
        for path in ["/api/state", "/api/services"] {
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
}
