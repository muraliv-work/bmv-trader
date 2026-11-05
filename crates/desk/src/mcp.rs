//! The agents' MCP tools (DESIGN §4c): a stdio JSON-RPC bridge that turns each tool call into one
//! Desk API call with the agent's own token. It decides nothing; the desk does. Owner-only routes
//! (approving, audit) have no tool, and arguments become a JSON body the desk checks strictly.

use serde_json::{Map, Value, json};

/// The protocol version answered when the client doesn't name one.
const PROTOCOL: &str = "2025-06-18";

/// One tool: name, what it's for, HTTP method, path (`{strategy}` is filled from the arguments),
/// and its arguments as (name, JSON type, description). `strategy` is always an integer path part.
struct Tool {
    name: &'static str,
    about: &'static str,
    method: &'static str,
    path: &'static str,
    args: &'static [(&'static str, &'static str, &'static str)],
}

const STRATEGY: (&str, &str, &str) = ("strategy", "integer", "the strategy id");

const TOOLS: &[Tool] = &[
    Tool {
        name: "whoami",
        about: "Your agent id and role on the desk.",
        method: "GET",
        path: "/v1/whoami",
        args: &[],
    },
    Tool {
        name: "list_strategies",
        about: "Every strategy with its lifecycle state and version.",
        method: "GET",
        path: "/v1/strategies",
        args: &[],
    },
    Tool {
        name: "create_strategy",
        about: "Researcher: register a new strategy idea (state idea).",
        method: "POST",
        path: "/v1/strategies",
        args: &[("name", "string", "1-100 plain characters")],
    },
    Tool {
        name: "set_criteria",
        about: "Strategist: pre-register the pass criteria, before any backtest.",
        method: "POST",
        path: "/v1/strategies/{strategy}/criteria",
        args: &[
            STRATEGY,
            ("criteria", "string", "the criteria as JSON text"),
        ],
    },
    Tool {
        name: "submit_version",
        about: "Developer: submit the code for the current version, by its hash.",
        method: "POST",
        path: "/v1/strategies/{strategy}/versions",
        args: &[STRATEGY, ("code_hash", "string", "e.g. sha256:...")],
    },
    Tool {
        name: "record_backtest",
        about: "Record a backtest run; only the Validator's official runs count for a gate.",
        method: "POST",
        path: "/v1/strategies/{strategy}/backtests",
        args: &[
            STRATEGY,
            (
                "official",
                "boolean",
                "true for the Validator's official run",
            ),
            ("inputs_hash", "string", "hash of the run's inputs"),
            ("results_hash", "string", "hash of the run's results"),
        ],
    },
    Tool {
        name: "record_paper",
        about: "Ops: record paper-trading days and trades for the current version.",
        method: "POST",
        path: "/v1/strategies/{strategy}/paper",
        args: &[
            STRATEGY,
            ("days", "integer", "trading days so far"),
            ("trades", "integer", "trades so far"),
        ],
    },
    Tool {
        name: "sign",
        about: "Sign the strategy's move to a lifecycle state. It moves once every required role has signed.",
        method: "POST",
        path: "/v1/strategies/{strategy}/sign",
        args: &[STRATEGY, ("to", "string", "target state, e.g. validated")],
    },
    Tool {
        name: "halt",
        about: "Halt a strategy. Anyone may; risk and compliance halts belong to those roles.",
        method: "POST",
        path: "/v1/strategies/{strategy}/halt",
        args: &[
            STRATEGY,
            ("reason", "string", "e.g. risk_breach, decay, incident"),
        ],
    },
    Tool {
        name: "tighten_limit",
        about: "Risk: tighten a risk limit. Loosening one is an owner request.",
        method: "POST",
        path: "/v1/strategies/{strategy}/limits",
        args: &[
            STRATEGY,
            ("name", "string", "the limit, e.g. max_daily_loss_usd"),
            ("value", "string", "the new value as a decimal string"),
        ],
    },
    Tool {
        name: "strategy_evidence",
        about: "A strategy's state, version and pre-registered criteria, its latest code-computed \
                evidence (backtest, stress battery, deflated Sharpe, reproduction hashes), each \
                criterion checked against it by the desk, and who has signed this version. Read-only.",
        method: "GET",
        path: "/v1/strategies/{strategy}/evidence",
        args: &[STRATEGY],
    },
    Tool {
        name: "strategy_limits",
        about: "A strategy's risk limits, each at its latest version.",
        method: "GET",
        path: "/v1/strategies/{strategy}/limits",
        args: &[STRATEGY],
    },
    Tool {
        name: "request_owner_action",
        about: "Ask the owner (Touch ID) for a human-only action; it waits for their signature.",
        method: "POST",
        path: "/v1/owner/requests",
        args: &[(
            "action",
            "object",
            "e.g. {\"kind\":\"transition\",\"strategy\":1,\"to\":\"limits_set\",\"version\":0}",
        )],
    },
    Tool {
        name: "pending_owner_requests",
        about: "Requests waiting for the owner.",
        method: "GET",
        path: "/v1/owner/requests",
        args: &[],
    },
];

fn tool_list() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            let props: Map<String, Value> = t
                .args
                .iter()
                .map(|(n, ty, d)| (n.to_string(), json!({"type": ty, "description": d})))
                .collect();
            let required: Vec<&str> = t.args.iter().map(|a| a.0).collect();
            json!({
                "name": t.name,
                "description": t.about,
                "inputSchema": {
                    "type": "object",
                    "properties": props,
                    "required": required,
                    "additionalProperties": false,
                },
            })
        })
        .collect();
    json!({"tools": tools})
}

/// One tool call: (method, path, body) for the desk.
fn route(name: &str, args: &Value) -> Result<(&'static str, String, Option<Value>), String> {
    let tool = TOOLS
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| format!("no tool {name:?}"))?;
    let mut body = match args {
        Value::Object(m) => m.clone(),
        Value::Null => Map::new(),
        _ => return Err("arguments must be an object".into()),
    };
    let mut path = tool.path.to_string();
    if path.contains("{strategy}") {
        let id = body
            .remove("strategy")
            .and_then(|v| v.as_i64())
            .ok_or("strategy must be an integer id")?;
        path = path.replace("{strategy}", &id.to_string());
    }
    let body = match tool.method {
        "GET" => None,
        // the owner request body is the action itself
        _ if tool.name == "request_owner_action" => {
            Some(body.remove("action").ok_or("action is required")?)
        }
        _ => Some(Value::Object(body)),
    };
    Ok((tool.method, path, body))
}

/// Makes one Desk API call: (method, path, body) to the JSON reply or the desk's refusal.
pub type DeskCall<'a> = &'a dyn Fn(&str, &str, Option<&Value>) -> Result<Value, String>;

/// Answers one JSON-RPC message; `None` for a notification. `desk` makes the Desk API call.
pub fn handle(msg: &Value, desk: DeskCall) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "bmv-desk", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tool_list()),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            // a refused call is a tool result the agent reads, not a protocol error
            let (text, is_error) = match route(name, &msg["params"]["arguments"])
                .and_then(|(m, p, b)| desk(m, &p, b.as_ref()))
            {
                Ok(v) => (v.to_string(), false),
                Err(e) => (e, true),
            };
            Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
        }
        _ => Err(json!({"code": -32601, "message": format!("no method {method:?}")})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn tools_map_to_desk_calls_and_refusals_reach_the_agent() {
        let calls = RefCell::new(Vec::new());
        let desk = |m: &str, p: &str, b: Option<&Value>| {
            calls
                .borrow_mut()
                .push((m.to_string(), p.to_string(), b.cloned()));
            if p.ends_with("/sign") {
                Err("not permitted: needs validator".to_string())
            } else {
                Ok(json!({"ok": true}))
            }
        };
        let call = |name: &str, args: Value| {
            handle(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": name, "arguments": args}}),
                &desk,
            )
            .unwrap()["result"]
                .clone()
        };

        let init = handle(
            &json!({"id": 0, "method": "initialize", "params": {}}),
            &desk,
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL);
        assert!(handle(&json!({"method": "notifications/initialized"}), &desk).is_none());
        let list = handle(&json!({"id": 2, "method": "tools/list"}), &desk).unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"sign") && !names.iter().any(|n| n.contains("approve")));

        assert_eq!(
            call("create_strategy", json!({"name": "gap fade"}))["isError"],
            false
        );
        let r = call("sign", json!({"strategy": 3, "to": "validated"}));
        assert_eq!(r["isError"], true);
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("needs validator")
        );
        call(
            "request_owner_action",
            json!({"action": {"kind": "issue_token", "agent": "x"}}),
        );
        {
            let c = calls.borrow();
            assert_eq!(
                c[0],
                (
                    "POST".into(),
                    "/v1/strategies".into(),
                    Some(json!({"name": "gap fade"}))
                )
            );
            assert_eq!(
                c[1],
                (
                    "POST".into(),
                    "/v1/strategies/3/sign".into(),
                    Some(json!({"to": "validated"}))
                )
            );
            assert_eq!(c[2].2, Some(json!({"kind": "issue_token", "agent": "x"})));
        }

        // a path can't be steered: the strategy must be an integer, the tool must exist
        let before = calls.borrow().len();
        assert_eq!(
            call("sign", json!({"strategy": "1/../../owner", "to": "x"}))["isError"],
            true
        );
        assert_eq!(call("approve_owner_action", json!({}))["isError"], true);
        assert_eq!(call("halt", json!("text"))["isError"], true);
        assert_eq!(
            calls.borrow().len(),
            before,
            "none of those reached the desk"
        );
        let bad = handle(&json!({"id": 9, "method": "resources/list"}), &desk).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
    }
}
