//! `desk-mcp`: the agents' MCP server over stdio (one JSON-RPC message per line). Paperclip starts
//! one per agent with that agent's DESK_TOKEN; every tool call becomes a Desk API call with it.

use desk::api::{self, DEFAULT_ADDR};
use desk::mcp;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let Ok(token) = std::env::var("DESK_TOKEN") else {
        eprintln!("desk-mcp: DESK_TOKEN (this agent's desk token) is not set");
        return ExitCode::FAILURE;
    };
    let addr = std::env::var("DESK_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.into());
    let desk = |method: &str, path: &str, body: Option<&Value>| {
        api::call(&addr, method, path, Some(&token), body)
    };
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => mcp::handle(&msg, &desk),
            Err(_) => Some(json!({"jsonrpc": "2.0", "id": null,
                                  "error": {"code": -32700, "message": "not JSON"}})),
        };
        if let Some(r) = reply
            && writeln!(out, "{r}").and_then(|_| out.flush()).is_err()
        {
            break;
        }
    }
    ExitCode::SUCCESS
}
