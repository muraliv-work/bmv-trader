//! `desk-ping <check> <program> [args…]`: run one launchd job's program, then ping its
//! healthchecks.io check (DECISION R13 step 0b, DESIGN §12), so a dead Mac, a dead job or a failed
//! run reaches the owner. launchd starts this binary, not a script: macOS won't let launchd's bash
//! read scripts in ~/Documents, and the program runs as this binary's child, under its access.

use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::Duration;

/// The ping body: the output's last 4 kB, enough to say what went wrong. Only the tail leaves the
/// Mac; the whole output stays in the launchd log.
const BODY_MAX: usize = 4_000;

/// The run's exit code (128 + the signal if one killed it), and the tail of its stdout and stderr
/// together. All of it is copied to this process's stdout, the launchd log, as it comes.
pub fn run(program: &str, args: &[String]) -> (i32, Vec<u8>) {
    let fail = |e: String| (127, format!("{e}\n").into_bytes());
    let (mut reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return fail(format!("desk-ping: pipe: {e}")),
    };
    let child = writer
        .try_clone()
        .map_err(|e| e.to_string())
        .and_then(|err| {
            Command::new(program)
                .args(args)
                .stdout(writer)
                .stderr(err)
                .spawn()
                .map_err(|e| format!("desk-ping: can't start {program}: {e}"))
        });
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    // the Command, holding the pipe's write ends, is gone, so read ends when the child exits
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut log = std::io::stdout();
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                out.extend_from_slice(format!("desk-ping: read: {e}\n").as_bytes());
                break;
            }
        };
        // a lost log line is still in the ping body
        let _ = log.write_all(&buf[..n]).and_then(|_| log.flush());
        out.extend_from_slice(&buf[..n]);
        if out.len() > 2 * BODY_MAX {
            out.drain(..out.len() - BODY_MAX);
        }
    }
    drop(reader); // after a read error, a child still writing gets EPIPE instead of blocking
    let code = match child.wait() {
        Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)),
        Err(e) => {
            out.extend_from_slice(format!("desk-ping: wait: {e}\n").as_bytes());
            1
        }
    };
    (code, out)
}

/// The check's slug: `<check>` on exit 0, `<check>/fail` otherwise.
pub fn slug(check: &str, code: i32) -> String {
    if code == 0 {
        check.to_string()
    } else {
        format!("{check}/fail")
    }
}

/// The last `BODY_MAX` bytes of the output.
pub fn body(out: &[u8]) -> &[u8] {
    &out[out.len().saturating_sub(BODY_MAX)..]
}

/// Runs the job, pings, and returns the job's exit code. A missing key or a failed ping is only
/// logged and never changes the exit code; the check's missed ping then raises the alert.
pub fn run_and_ping(check: &str, program: &str, args: &[String]) -> i32 {
    let (code, out) = run(program, args);
    let slug = slug(check, code);
    if code != 0 {
        eprintln!("desk-ping: {program} failed with exit code {code}");
    }
    match key() {
        Ok(key) => {
            if let Err(e) = send(&key, &slug, body(&out)) {
                eprintln!("desk-ping: ping {slug} failed: {e}");
            }
        }
        Err(e) => eprintln!("desk-ping: {e}, ping {slug} skipped"),
    }
    code
}

/// The ping key, Keychain bmv-trader / HEALTHCHECKS_PING_KEY.
fn key() -> Result<String, String> {
    let out = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "bmv-trader",
            "-a",
            "HEALTHCHECKS_PING_KEY",
            "-w",
        ])
        .output()
        .map_err(|e| format!("Keychain: {e}"))?;
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || key.is_empty() {
        return Err(format!(
            "no HEALTHCHECKS_PING_KEY from the Keychain ({}: {})",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(key)
}

/// Up to three tries, 10 s each; an HTTP error (a bad key or slug) isn't retried. The error never
/// carries the URL, which holds the key.
fn send(key: &str, slug: &str, body: &[u8]) -> Result<(), String> {
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none()) // a redirect would carry the key
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("https://hc-ping.com/{key}/{slug}");
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        match http.post(&url).body(body.to_vec()).send() {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => return Err(format!("HTTP {}", r.status())),
            Err(e) => last = e.without_url().to_string(),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> (i32, Vec<u8>) {
        run("/bin/sh", &["-c".into(), script.into()])
    }

    #[test]
    fn passes_the_exit_code_on_and_keeps_stdout_and_stderr() {
        let (code, out) = sh("echo ok; echo bad >&2; exit 3");
        assert_eq!(code, 3);
        assert_eq!(String::from_utf8_lossy(&out), "ok\nbad\n");
        assert_eq!(sh("true").0, 0);
        assert_eq!(sh("kill -9 $$").0, 137);
    }

    #[test]
    fn a_long_run_keeps_only_the_tail() {
        let (code, out) = sh("yes | head -c 1000000; echo end; exit 2");
        assert_eq!(code, 2);
        assert!(out.len() <= 2 * BODY_MAX && out.ends_with(b"y\nend\n"));
    }

    #[test]
    fn a_program_that_cannot_start_fails_with_127_and_says_why() {
        let (code, out) = run("/nonexistent/bmv-trial10", &[]);
        assert_eq!(code, 127);
        assert!(String::from_utf8_lossy(&out).contains("can't start /nonexistent/bmv-trial10"));
    }

    #[test]
    fn pings_the_fail_slug_unless_exit_0_and_sends_only_the_tail() {
        assert_eq!(slug("desk-run", 0), "desk-run");
        assert_eq!(slug("desk-run", 1), "desk-run/fail");
        assert_eq!(slug("reconcile", 127), "reconcile/fail");
        let long = vec![b'x'; BODY_MAX + 5];
        assert_eq!(body(&long).len(), BODY_MAX);
        assert_eq!(body(b"short"), b"short");
    }
}
