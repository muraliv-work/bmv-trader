//! The agents' runner (DESIGN §4c, S36). Paperclip starts `desk-agent` on each heartbeat; this is
//! its loop. The model is offered only its role's desk tools (the same ones `desk-mcp` serves), so
//! it has no shell, files or web. Every reply's model is checked against the charter, every call
//! is charged to the agent's desk budget, and free or local models are never offered `sign` (A5).

use crate::mcp::{self, DeskCall};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use std::str::FromStr;

/// One agent's charter: written by the owner, never by the agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Charter {
    /// The desk agent id; the token must belong to it.
    pub agent: String,
    pub provider: Provider,
    /// Pinned exactly; a reply from any other model stops the run (signers).
    pub model: String,
    /// The role's standing instructions.
    pub prompt: String,
    /// What each heartbeat asks of it.
    pub task: String,
    /// The desk tools it is offered; anything else is refused.
    pub tools: Vec<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
}

fn default_max_tokens() -> u32 {
    8000
}

fn default_max_turns() -> u32 {
    8
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Claude, called directly: paid, model checked, may sign.
    Anthropic,
    /// OmniRoute's local route: free, authors only, never signs (A5).
    Omniroute,
}

impl Charter {
    pub fn parse(text: &str) -> Result<Charter, String> {
        let c: Charter = serde_json::from_str(text).map_err(|e| format!("charter: {e}"))?;
        if c.agent.is_empty()
            || !c
                .agent
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(format!(
                "charter: agent {:?} isn't a desk agent id",
                c.agent
            ));
        }
        if !(1..=16_000).contains(&c.max_tokens) || !(1..=20).contains(&c.max_turns) {
            return Err("charter: max_tokens is 1-16000 and max_turns 1-20".into());
        }
        if c.provider == Provider::Anthropic && price(&c.model, 0).is_none() {
            return Err(format!("charter: no price for {}, so no budget", c.model));
        }
        Ok(c)
    }

    /// The tools this agent is offered: its charter's, and never `sign` from a free model.
    fn offers(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t == name)
            && !(name == "sign" && self.provider != Provider::Anthropic)
    }
}

/// USD per million tokens (input, output). `input` picks Haiku's long-prompt tier.
fn price(model: &str, input: u64) -> Option<(Decimal, Decimal)> {
    let d = |s| Decimal::from_str(s).ok();
    match model {
        "claude-sonnet-5-5" => Some((d("2")?, d("10")?)),
        "claude-haiku-5-5" if input > 100_000 => Some((d("0.50")?, d("2.50")?)),
        "claude-haiku-5-5" => Some((d("0.10")?, d("0.50")?)),
        _ => None,
    }
}

fn cost(model: &str, input: u64, output: u64) -> Decimal {
    let (i, o) = price(model, input).unwrap_or_default();
    (i * Decimal::from(input) + o * Decimal::from(output)) / Decimal::from(1_000_000)
}

/// What one model call used, for the books.
#[derive(Debug, PartialEq)]
pub struct Usage {
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub usd: Decimal,
}

/// The tool calls a reply asks for: (id, name, arguments).
type Calls = Vec<(String, String, Result<Value, String>)>;

/// The longest tool result handed back to the model; desk data is untrusted text.
const MAX_RESULT_CHARS: usize = 20_000;

/// What a reply cost, from the usage it reports. `None` when a paid reply has no usage: the caller
/// books the worst case, so nothing goes unbooked.
fn usage(c: &Charter, r: &Value) -> Option<Usage> {
    let model = r["model"].as_str().unwrap_or("").to_string();
    let n = |v: &Value| v.as_u64();
    match c.provider {
        Provider::Anthropic => {
            let u = &r["usage"];
            let input = n(&u["input_tokens"])?
                + n(&u["cache_creation_input_tokens"]).unwrap_or(0)
                + n(&u["cache_read_input_tokens"]).unwrap_or(0);
            let output = n(&u["output_tokens"])?;
            // priced as the model that answered, when it has a price
            let priced = if price(&model, input).is_some() {
                &model
            } else {
                &c.model
            };
            let usd = cost(priced, input, output);
            Some(Usage {
                model,
                input,
                output,
                usd,
            })
        }
        Provider::Omniroute => Some(Usage {
            model,
            input: n(&r["usage"]["prompt_tokens"]).unwrap_or(0),
            output: n(&r["usage"]["completion_tokens"]).unwrap_or(0),
            usd: Decimal::ZERO,
        }),
    }
}

fn named(id: &Value, name: &Value) -> Result<(String, String), String> {
    match (id.as_str(), name.as_str()) {
        (Some(i), Some(n)) if !i.is_empty() && !n.is_empty() => Ok((i.into(), n.into())),
        _ => Err("the model sent a tool call with no id or name".into()),
    }
}

/// The reply's text and tool calls. A reply that stopped early, or isn't the expected shape, is an
/// error: the run fails rather than pass as done.
fn turn(provider: Provider, r: &Value) -> Result<(String, Calls), String> {
    let mut text = String::new();
    let mut calls = vec![];
    match provider {
        Provider::Anthropic => {
            let stop = r["stop_reason"].as_str().unwrap_or("");
            if !matches!(stop, "tool_use" | "end_turn" | "stop_sequence") {
                return Err(format!("the model stopped early ({stop:?})"));
            }
            for b in r["content"].as_array().ok_or("the model sent no content")? {
                match b["type"].as_str() {
                    Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
                    Some("tool_use") => {
                        let (id, name) = named(&b["id"], &b["name"])?;
                        calls.push((id, name, Ok(b["input"].clone())));
                    }
                    _ => {}
                }
            }
            if stop != "tool_use" {
                calls.clear();
            }
        }
        Provider::Omniroute => {
            let choice = &r["choices"][0];
            let m = &choice["message"];
            if !m.is_object() {
                return Err("the model sent no message".into());
            }
            if choice["finish_reason"] == "length" {
                return Err("the model stopped early (length)".into());
            }
            text = m["content"].as_str().unwrap_or("").to_string();
            for c in m["tool_calls"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let (id, name) = named(&c["id"], &c["function"]["name"])?;
                let args = c["function"]["arguments"].as_str().unwrap_or("{}");
                let args = serde_json::from_str(args).map_err(|_| "arguments aren't JSON".into());
                calls.push((id, name, args));
            }
        }
    }
    Ok((text, calls))
}

/// What a heartbeat did.
#[derive(Debug)]
pub struct Outcome {
    pub text: String,
    pub turns: u32,
    pub usd: Decimal,
    pub tools: Vec<String>,
}

/// One heartbeat. `post` sends a request body to the charter's provider; `report` hears each
/// call's usage (for Paperclip). The desk is called with the agent's own token.
pub fn run(
    c: &Charter,
    desk: DeskCall,
    post: &dyn Fn(&Value) -> Result<Value, String>,
    report: &mut dyn FnMut(&Usage),
) -> Result<Outcome, String> {
    let me = desk("GET", "/v1/whoami", None)?;
    if me["agent"] != c.agent.as_str() {
        return Err(format!(
            "the desk token belongs to {}, not {}",
            me["agent"], c.agent
        ));
    }
    let listed = mcp::handle(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        desk,
    )
    .ok_or("no tool list")?;
    let tools: Vec<&Value> = listed["result"]["tools"]
        .as_array()
        .ok_or("no tool list")?
        .iter()
        .filter(|t| t["name"].as_str().is_some_and(|n| c.offers(n)))
        .collect();
    if let Some(missing) = c
        .tools
        .iter()
        .find(|n| !tools.iter().any(|t| t["name"] == n.as_str()))
        && c.offers(missing)
    {
        return Err(format!("charter: the desk has no tool {missing}"));
    }
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| match c.provider {
            Provider::Anthropic => json!({"name": t["name"], "description": t["description"],
                                          "input_schema": t["inputSchema"]}),
            Provider::Omniroute => json!({"type": "function", "function": {
                "name": t["name"], "description": t["description"], "parameters": t["inputSchema"]}}),
        })
        .collect();

    let mut messages = match c.provider {
        Provider::Anthropic => vec![json!({"role": "user", "content": c.task})],
        Provider::Omniroute => vec![
            json!({"role": "system", "content": c.prompt}),
            json!({"role": "user", "content": c.task}),
        ],
    };
    let mut out = Outcome {
        text: String::new(),
        turns: 0,
        usd: Decimal::ZERO,
        tools: vec![],
    };
    while out.turns < c.max_turns {
        out.turns += 1;
        let body = match c.provider {
            Provider::Anthropic => json!({"model": c.model, "max_tokens": c.max_tokens,
                "system": c.prompt, "messages": messages, "tools": tools}),
            Provider::Omniroute => json!({"model": c.model, "max_tokens": c.max_tokens,
                "messages": messages, "tools": tools}),
        };
        let worst = match c.provider {
            Provider::Anthropic => room(c, desk, &body)?,
            Provider::Omniroute => Decimal::ZERO,
        };
        let reply = post(&body)?;
        // booked first, whatever the reply says next: a wrong or broken reply still cost money
        let used = usage(c, &reply);
        let usd = used.as_ref().map_or(worst, |u| u.usd);
        if usd > Decimal::ZERO {
            desk(
                "POST",
                "/v1/budget/charge",
                Some(&json!({"usd": usd.to_string()})),
            )?;
        }
        out.usd += usd;
        let Some(used) = used else {
            return Err(format!(
                "the reply had no usage; booked the worst case, ${}",
                usd.round_dp(4)
            ));
        };
        report(&used);
        if c.provider == Provider::Anthropic && used.model != c.model {
            return Err(format!(
                "the reply came from {}, not the pinned {}",
                used.model, c.model
            ));
        }
        let (text, calls) = turn(c.provider, &reply)?;
        out.text = text;
        if calls.is_empty() {
            return Ok(out);
        }
        messages.push(match c.provider {
            Provider::Anthropic => json!({"role": "assistant", "content": reply["content"]}),
            Provider::Omniroute => reply["choices"][0]["message"].clone(),
        });
        let mut results = vec![];
        for (id, name, args) in calls {
            out.tools.push(name.clone());
            let (text, failed) = match args {
                _ if !c.offers(&name) => (format!("{name} isn't one of your tools"), true),
                Err(e) => (e, true),
                Ok(args) => {
                    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                      "params": {"name": name, "arguments": args}});
                    let r = mcp::handle(&call, desk).ok_or("no tool reply")?;
                    let r = &r["result"];
                    (
                        r["content"][0]["text"].as_str().unwrap_or("").to_string(),
                        r["isError"] == true,
                    )
                }
            };
            let mut text: String = text.chars().take(MAX_RESULT_CHARS).collect();
            if text.is_empty() {
                text = if failed { "the tool failed" } else { "done" }.into();
            }
            match c.provider {
                Provider::Anthropic => results.push(json!({"type": "tool_result",
                    "tool_use_id": id, "content": text, "is_error": failed})),
                Provider::Omniroute => {
                    messages.push(json!({"role": "tool", "tool_call_id": id, "content": text}))
                }
            }
        }
        if c.provider == Provider::Anthropic {
            messages.push(json!({"role": "user", "content": results}));
        }
    }
    // the last turn's tools ran but the model never saw their results: not a finished heartbeat
    Err(format!(
        "stopped after {} turns without finishing (tools run: {})",
        out.turns,
        out.tools.join(", ")
    ))
}

/// Refuses a call the budget can't cover in the worst case: the whole request as input (at about
/// two bytes a token, an overestimate) and every allowed output token.
fn room(c: &Charter, desk: DeskCall, body: &Value) -> Result<Decimal, String> {
    let b = desk("GET", "/v1/budget", None)?;
    let d = |k: &str| Decimal::from_str(b[k].as_str().unwrap_or("x")).map_err(|_| "bad budget");
    let (limit, spent) = (d("limit_usd")?, d("spent_usd")?);
    let input = body.to_string().len() as u64 / 2;
    let worst = cost(&c.model, input, u64::from(c.max_tokens));
    if spent + worst > limit {
        return Err(format!(
            "budget: ${spent} of ${limit} spent; the next call could cost ${}",
            worst.round_dp(4)
        ));
    }
    Ok(worst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn charter(provider: &str, model: &str, tools: &[&str]) -> Charter {
        Charter::parse(
            &json!({"agent": "val-1", "provider": provider, "model": model,
                    "prompt": "You are the Validator.", "task": "Check the desk.",
                    "tools": tools})
            .to_string(),
        )
        .unwrap()
    }

    /// A fake desk: whoami, the budget, and a log of every other call.
    fn desk<'a>(
        log: &'a RefCell<Vec<String>>,
        spent: &'static str,
    ) -> impl Fn(&str, &str, Option<&Value>) -> Result<Value, String> + 'a {
        move |method, path, body| {
            log.borrow_mut().push(format!(
                "{method} {path} {}",
                body.map(|b| b.to_string()).unwrap_or_default()
            ));
            Ok(match path {
                "/v1/whoami" => json!({"agent": "val-1", "role": "validator"}),
                "/v1/budget" => json!({"limit_usd": "35", "spent_usd": spent}),
                "/v1/strategies" => json!([]),
                _ => json!({"ok": true}),
            })
        }
    }

    fn claude(model: &str, content: Value, stop: &str) -> Value {
        json!({"model": model, "content": content, "stop_reason": stop,
               "usage": {"input_tokens": 1000, "output_tokens": 500}})
    }

    #[test]
    fn a_heartbeat_uses_only_its_tools_and_pays_for_every_call() {
        let c = charter(
            "anthropic",
            "claude-sonnet-5-5",
            &["list_strategies", "sign"],
        );
        let log = RefCell::new(vec![]);
        let d = desk(&log, "0");
        let sent = RefCell::new(vec![]);
        let replies = RefCell::new(vec![
            claude(
                "claude-sonnet-5-5",
                json!([
                    {"type": "tool_use", "id": "t1", "name": "list_strategies", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "halt", "input": {"strategy": 1, "reason": "x"}}
                ]),
                "tool_use",
            ),
            claude(
                "claude-sonnet-5-5",
                json!([{"type": "text", "text": "Nothing to review."}]),
                "end_turn",
            ),
        ]);
        let post = |b: &Value| {
            sent.borrow_mut().push(b.clone());
            Ok(replies.borrow_mut().remove(0))
        };
        let mut usage = vec![];
        let out = run(&c, &d, &post, &mut |u| usage.push(u.model.clone())).unwrap();
        assert_eq!(out.text, "Nothing to review.");
        assert_eq!(out.turns, 2);
        // 1000 in at $2/M + 500 out at $10/M = $0.007, twice
        assert_eq!(out.usd, Decimal::from_str("0.014").unwrap());
        assert_eq!(usage.len(), 2);
        let offered: Vec<String> = sent.borrow()[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(offered, ["list_strategies", "sign"]);
        // the model's unoffered `halt` was refused, never sent to the desk
        let log = log.borrow();
        assert!(log.iter().any(|l| l.starts_with("GET /v1/strategies")));
        assert!(!log.iter().any(|l| l.contains("/halt")));
        assert_eq!(
            log.iter()
                .filter(|l| l.starts_with("POST /v1/budget/charge"))
                .count(),
            2
        );
        let second = &sent.borrow()[1]["messages"];
        assert_eq!(second[2]["content"][1]["is_error"], true);
    }

    #[test]
    fn a_different_model_stops_the_run_after_paying() {
        let c = charter("anthropic", "claude-sonnet-5-5", &["list_strategies"]);
        let log = RefCell::new(vec![]);
        let d = desk(&log, "0");
        let post = |_: &Value| Ok(claude("claude-haiku-5-5", json!([]), "end_turn"));
        let err = run(&c, &d, &post, &mut |_| {}).unwrap_err();
        assert!(err.contains("not the pinned"), "{err}");
        assert!(
            log.borrow()
                .iter()
                .any(|l| l.starts_with("POST /v1/budget/charge"))
        );
    }

    #[test]
    fn no_room_in_the_budget_means_no_call() {
        let c = charter("anthropic", "claude-sonnet-5-5", &[]);
        let log = RefCell::new(vec![]);
        let d = desk(&log, "34.99");
        let post = |_: &Value| -> Result<Value, String> { panic!("must not call the model") };
        assert!(
            run(&c, &d, &post, &mut |_| {})
                .unwrap_err()
                .starts_with("budget")
        );
    }

    #[test]
    fn a_free_model_is_never_offered_sign() {
        let c = charter("omniroute", "bmv-local", &["list_strategies", "sign"]);
        let log = RefCell::new(vec![]);
        let d = desk(&log, "0");
        let sent = RefCell::new(vec![]);
        let replies = RefCell::new(vec![
            json!({"model": "bmv-local", "choices": [{"message": {"role": "assistant", "content": "",
                "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "sign", "arguments": "{\"strategy\": 1, \"to\": \"validated\"}"}}]}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
            json!({"model": "bmv-local", "choices": [{"message": {"role": "assistant", "content": "done"}}]}),
        ]);
        let post = |b: &Value| {
            sent.borrow_mut().push(b.clone());
            Ok(replies.borrow_mut().remove(0))
        };
        let out = run(&c, &d, &post, &mut |_| {}).unwrap();
        assert_eq!(out.usd, Decimal::ZERO);
        let offered = sent.borrow()[0]["tools"].to_string();
        assert!(!offered.contains("\"sign\""), "{offered}");
        let log = log.borrow();
        assert!(
            !log.iter()
                .any(|l| l.contains("/sign") || l.contains("budget"))
        );
        assert!(
            sent.borrow()[1]["messages"]
                .to_string()
                .contains("isn't one of your tools")
        );
    }

    #[test]
    fn charters_are_checked() {
        let base = json!({"agent": "val-1", "provider": "anthropic", "model": "claude-sonnet-5-5",
                          "prompt": "p", "task": "t", "tools": []});
        assert!(Charter::parse(&base.to_string()).is_ok());
        let with = |k: &str, v: Value| {
            let mut b = base.clone();
            b[k] = v;
            Charter::parse(&b.to_string())
        };
        assert!(
            with("model", json!("claude-opus-9")).is_err(),
            "no price, no budget"
        );
        assert!(with("agent", json!("val 1; rm")).is_err());
        assert!(with("max_turns", json!(500)).is_err());
        assert!(
            with("shell", json!(true)).is_err(),
            "unknown fields refused"
        );
    }

    /// Runs one heartbeat against canned replies; returns the result, the requests sent, and the
    /// desk log.
    fn replay(
        c: &Charter,
        replies: Vec<Value>,
    ) -> (Result<Outcome, String>, Vec<Value>, Vec<String>) {
        let log = RefCell::new(vec![]);
        let d = desk(&log, "0");
        let sent = RefCell::new(vec![]);
        let replies = RefCell::new(replies);
        let post = |b: &Value| {
            sent.borrow_mut().push(b.clone());
            Ok(replies.borrow_mut().remove(0))
        };
        let r = run(c, &d, &post, &mut |_| {});
        let log = log.borrow().clone();
        (r, sent.into_inner(), log)
    }

    #[test]
    fn thinking_and_tool_ids_go_back_unchanged() {
        let c = charter("anthropic", "claude-sonnet-5-5", &["list_strategies"]);
        let content = json!([
            {"type": "thinking", "thinking": "hmm", "signature": "sig"},
            {"type": "tool_use", "id": "toolu_9", "name": "list_strategies", "input": {}}
        ]);
        let (r, sent, _) = replay(
            &c,
            vec![
                claude("claude-sonnet-5-5", content.clone(), "tool_use"),
                claude(
                    "claude-sonnet-5-5",
                    json!([{"type": "text", "text": "ok"}]),
                    "end_turn",
                ),
            ],
        );
        r.unwrap();
        let m = &sent[1]["messages"];
        assert_eq!(m[1]["content"], content);
        assert_eq!(m[2]["content"][0]["tool_use_id"], "toolu_9");
        assert_eq!(m[2]["content"][0]["is_error"], false);
    }

    #[test]
    fn an_early_stop_or_a_broken_reply_fails_the_run_but_is_booked() {
        let c = charter("anthropic", "claude-sonnet-5-5", &[]);
        let (r, _, log) = replay(
            &c,
            vec![claude("claude-sonnet-5-5", json!([]), "max_tokens")],
        );
        assert!(r.unwrap_err().contains("stopped early"));
        assert!(log.iter().any(|l| l.starts_with("POST /v1/budget/charge")));
        // no usage: the worst case is booked, then the run fails
        let (r, _, log) = replay(
            &c,
            vec![json!({"model": "claude-sonnet-5-5", "content": [],
                                                  "stop_reason": "end_turn"})],
        );
        assert!(r.unwrap_err().contains("no usage"));
        assert!(log.iter().any(|l| l.starts_with("POST /v1/budget/charge")));
        let o = charter("omniroute", "bmv-local", &[]);
        let (r, _, _) = replay(&o, vec![json!({"error": "upstream down"})]);
        assert!(r.unwrap_err().contains("no message"));
    }

    #[test]
    fn running_out_of_turns_is_a_failure() {
        let mut c = charter("anthropic", "claude-sonnet-5-5", &["list_strategies"]);
        c.max_turns = 2;
        let call = || {
            claude(
                "claude-sonnet-5-5",
                json!([
            {"type": "tool_use", "id": "t", "name": "list_strategies", "input": {}}]),
                "tool_use",
            )
        };
        let (r, _, _) = replay(&c, vec![call(), call()]);
        assert!(r.unwrap_err().contains("without finishing"));
    }

    #[test]
    fn the_local_route_runs_tools_in_its_own_format() {
        let c = charter("omniroute", "bmv-local", &["list_strategies"]);
        let call = |id: &str, args: &str| {
            json!({"model": "bmv-local", "choices": [{"message": {
            "role": "assistant", "content": "",
            "tool_calls": [{"id": id, "type": "function",
                            "function": {"name": "list_strategies", "arguments": args}}]}}]})
        };
        let done = json!({"model": "bmv-local", "choices": [{"message": {"role": "assistant",
                          "content": "two"}, "finish_reason": "stop"}]});
        let (r, sent, log) = replay(&c, vec![call("c1", "{}"), call("c2", "not json"), done]);
        assert_eq!(r.unwrap().text, "two");
        let m = &sent[2]["messages"];
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "c1");
        assert_eq!(m[5]["content"], "arguments aren't JSON");
        assert_eq!(
            log.iter()
                .filter(|l| l.starts_with("GET /v1/strategies"))
                .count(),
            1
        );
    }
}
