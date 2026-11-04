//! `desk`: the Quant Desk server, and the owner's command for it (DESIGN §4c).
//!
//!   desk serve [--db PATH] [--port N]   run the Desk API on 127.0.0.1
//!   desk init [--db PATH] [--as USER]   make your Secure Enclave key (Touch ID) and register it;
//!                                       with --as, the database is written as USER via sudo
//!   desk pending                       requests waiting for you
//!   desk request '<json>'               ask for an owner action yourself, e.g.
//!                                       '{"kind":"add_agent","id":"val-1","role":"validator"}'
//!   desk sign <id>                      approve one request with Touch ID
//!
//! The owner's token comes from DESK_TOKEN, else the Keychain item `bmv-desk` / `board`. The key
//! lives at ~/.bmv-desk/owner.key (only usable on this Mac's Secure Enclave, with your finger).
//! `desk-sign` is run from next to this binary, never from a path an agent could set.

use desk::api::{self, DEFAULT_ADDR};
use desk::owner::{INIT_TEXT, Pending, signed_text};
use desk::store::Store;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run = match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("init") => init(&args[1..]),
        // what `init` runs as the database's owner
        Some("register") if args.len() == 4 => register(&args[1..]),
        Some("pending") => pending(),
        Some("request") if args.len() == 2 => request(&args[1]),
        Some("sign") if args.len() == 2 => sign(&args[1]),
        _ => Err("usage: desk serve [--db PATH] [--port N] | init [--db PATH] [--as USER] | pending | request '<json>' | sign <id>".into()),
    };
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("desk: {e}");
            ExitCode::FAILURE
        }
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

const DEFAULT_DB: &str = "data/desk/desk.db";

/// Opens (creating if needed) the database at a plain path.
async fn open(db: &str) -> Result<Store, String> {
    if db.contains(['?', '#', '%']) {
        return Err(format!("{db}: a database path can't contain ? # or %"));
    }
    if let Some(dir) = std::path::Path::new(db).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Store::open(&format!("sqlite://{db}?mode=rwc"))
        .await
        .map_err(|e| format!("{db}: {e}"))
}

fn serve(args: &[String]) -> Result<(), String> {
    let db = flag(args, "--db").unwrap_or(DEFAULT_DB);
    let port: u16 = flag(args, "--port")
        .unwrap_or("8790")
        .parse()
        .map_err(|_| "--port takes a number")?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let store = open(db).await?;
        // loopback only: the desk is never on the network
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| format!("127.0.0.1:{port}: {e}"))?;
        eprintln!("desk: serving {db} on 127.0.0.1:{port}");
        api::serve(listener, store).await.map_err(|e| e.to_string())
    })
}

fn addr() -> String {
    std::env::var("DESK_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.into())
}

fn key_file() -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    Ok(format!("{home}/.bmv-desk/owner.key"))
}

/// The signer built by scripts/build-desk-sign.sh, next to this binary.
fn desk_sign(args: &[&str], stdin: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = exe.with_file_name("desk-sign");
    let bin = path.display();
    let mut child = Command::new(&path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{bin}: {e} (build it with scripts/build-desk-sign.sh)"))?;
    child
        .stdin
        .take()
        .ok_or("desk-sign stdin")?
        .write_all(stdin.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("desk-sign refused (Touch ID cancelled?)".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn token() -> Result<String, String> {
    if let Ok(t) = std::env::var("DESK_TOKEN") {
        return Ok(t);
    }
    let out = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "bmv-desk",
            "-a",
            "board",
            "-w",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(
            "no owner token: set DESK_TOKEN or add the Keychain item bmv-desk/board".into(),
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Makes the key, signs the registration with Touch ID, then registers it straight into the
/// database (as `--as USER` through sudo when the desk runs as its own user). There is no HTTP
/// way in, so no other process can claim an empty desk first.
fn init(args: &[String]) -> Result<(), String> {
    let db = flag(args, "--db").unwrap_or(DEFAULT_DB);
    let key = key_file()?;
    if std::path::Path::new(&key).exists() {
        return Err(format!(
            "{key} already exists; it may be the registered owner key, so it is never replaced here"
        ));
    }
    let public = desk_sign(&["keygen", &key], "")?;
    println!("Secure Enclave key made: {key}\npublic key: {public}\nTouch ID now registers it.");
    let signature = desk_sign(&["sign", &key], INIT_TEXT)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = match flag(args, "--as") {
        Some(user) => {
            let mut c = Command::new("sudo");
            c.args(["-u", user]).arg(&exe);
            c
        }
        None => Command::new(&exe),
    };
    let status = cmd
        .args(["register", db, &public, &signature])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("registering the owner failed (see above)".into());
    }
    Ok(())
}

/// Registers the owner in the database directly; `init` runs this as the database's owner.
fn register(args: &[String]) -> Result<(), String> {
    let (db, public, signature) = (&args[0], &args[1], &args[2]);
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let token = rt.block_on(async {
        let store = open(db).await?;
        store
            .init_owner(public, signature)
            .await
            .map_err(|e| e.to_string())
    })?;
    println!(
        "You are the owner. Your desk token, shown once:\n\n  {token}\n\nStore it in the Keychain (it prompts for the token):\n  security add-generic-password -s bmv-desk -a board -w"
    );
    Ok(())
}

fn requests() -> Result<Vec<Pending>, String> {
    let v = api::call(&addr(), "GET", "/v1/owner/requests", Some(&token()?), None)?;
    serde_json::from_value(v).map_err(|e| format!("the desk sent a bad request list: {e}"))
}

fn pending() -> Result<(), String> {
    let list = requests()?;
    if list.is_empty() {
        println!("Nothing waiting for you.");
    }
    for p in list {
        println!(
            "{}  {}  (asked by {}, expires {})",
            p.id,
            p.action.describe(),
            p.requested_by,
            p.expires
        );
    }
    Ok(())
}

fn request(body: &str) -> Result<(), String> {
    let action: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    let p = api::call(
        &addr(),
        "POST",
        "/v1/owner/requests",
        Some(&token()?),
        Some(&action),
    )?;
    println!(
        "Request {} is waiting. Approve it with: desk sign {}",
        p["id"].as_str().unwrap_or("?"),
        p["id"].as_str().unwrap_or("?")
    );
    Ok(())
}

fn sign(id: &str) -> Result<(), String> {
    let p = requests()?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or("no such open request (it may have expired)")?;
    // what you see is what you sign: the text is rebuilt here from the action shown
    let text = signed_text(&p.id, &p.action, &p.expires);
    if text != p.text {
        return Err("the desk's text doesn't match the action; not signing".into());
    }
    println!(
        "{}\nasked by {}\nTouch ID to approve.",
        p.action.describe(),
        p.requested_by
    );
    let signature = desk_sign(&["sign", &key_file()?], &text)?;
    let r = api::call(
        &addr(),
        "POST",
        &format!("/v1/owner/requests/{id}/approve"),
        Some(&token()?),
        Some(&json!({"signature": signature})),
    )?;
    println!("Approved.");
    if let Some(t) = r["token"].as_str() {
        println!("The new token, shown once (give it to that agent only):\n\n  {t}");
    }
    Ok(())
}
