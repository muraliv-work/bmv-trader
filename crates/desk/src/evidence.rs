//! A strategy's code-computed evidence (DECISION S46). Only the engine posts it (`Role::Engine`);
//! the desk hashes what it is given, so the results hash is the desk's own, and checks each
//! pre-registered criterion against the posted numbers here, in code. A signer reads the outcome;
//! no agent types a number.
//!
//! - **Body:** a JSON object with a `metrics` object of numbers (plus anything descriptive, such
//!   as the stress battery's own checks). At most `MAX_BODY` bytes.
//! - **Criteria:** checkable when they read `{"checks": [{"metric": NAME, "min": X} | {"metric":
//!   NAME, "max": X}, ...]}`. Others are shown as written and marked "not checkable".

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The largest evidence body the desk keeps, in bytes of canonical JSON.
pub const MAX_BODY: usize = 48 * 1024;

/// The longest criteria text, in characters: a signer's model reads it.
pub const MAX_CRITERIA: usize = 4096;

/// The body fields a signer sees: those the engine writes (`stress::desk_evidence`).
pub const SHOWN: [&str; 8] = [
    "source", "trial", "name", "period", "units", "verdict", "checks", "metrics",
];

/// The body's canonical text: `serde_json`'s maps sort their keys, so equal bodies hash equal.
pub fn canonical(body: &Value) -> String {
    body.to_string()
}

pub fn hash(body: &Value) -> String {
    hex(&Sha256::digest(canonical(body).as_bytes()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Refuses a body the desk can't check: not an object, no `metrics` of finite numbers, too big.
pub fn validate(body: &Value) -> Result<(), String> {
    let o = body.as_object().ok_or("evidence must be a JSON object")?;
    let m = o
        .get("metrics")
        .and_then(Value::as_object)
        .ok_or("evidence needs a \"metrics\" object")?;
    if m.is_empty() {
        return Err("evidence has no metrics".into());
    }
    for (k, v) in m {
        if !v.as_f64().is_some_and(f64::is_finite) {
            return Err(format!("metric {k} isn't a finite number"));
        }
    }
    if canonical(body).len() > MAX_BODY {
        return Err(format!("evidence is over {MAX_BODY} bytes"));
    }
    Ok(())
}

/// One criterion's outcome: the metric it names, its rule, the posted value and whether it passed.
#[derive(Debug, PartialEq, Serialize)]
pub struct Outcome {
    pub metric: String,
    pub rule: String,
    pub value: Option<f64>,
    /// None when the evidence has no such metric: that's a fail for signing, shown as missing.
    pub passed: Option<bool>,
}

/// Each pre-registered criterion against the metrics, or None when the criteria aren't in the
/// checkable form.
pub fn evaluate(criteria: &str, metrics: &Map<String, Value>) -> Option<Vec<Outcome>> {
    let c: Value = serde_json::from_str(criteria).ok()?;
    let checks = c.get("checks")?.as_array()?;
    checks
        .iter()
        .map(|k| {
            let metric = k.get("metric")?.as_str()?.to_string();
            let (rule, test): (String, Box<dyn Fn(f64) -> bool>) = match (
                k.get("min").and_then(Value::as_f64),
                k.get("max").and_then(Value::as_f64),
            ) {
                (Some(lo), None) => (format!(">= {lo}"), Box::new(move |v| v >= lo)),
                (None, Some(hi)) => (format!("<= {hi}"), Box::new(move |v| v <= hi)),
                _ => return None,
            };
            let value = metrics.get(&metric).and_then(Value::as_f64);
            Some(Outcome {
                passed: value.map(&test),
                metric,
                rule,
                value,
            })
        })
        .collect()
}

/// True only when every criterion has its metric and passes.
pub fn all_pass(outcomes: &[Outcome]) -> bool {
    !outcomes.is_empty() && outcomes.iter().all(|o| o.passed == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn criteria_are_checked_in_code() {
        let criteria = r#"{"checks": [{"metric": "deflated_sharpe", "min": 0.95},
                                      {"metric": "max_drawdown_r", "max": 20},
                                      {"metric": "expectancy", "min": 0.05}]}"#;
        let m = json!({"deflated_sharpe": 0.97, "max_drawdown_r": 25.0});
        let out = evaluate(criteria, m.as_object().unwrap()).unwrap();
        assert_eq!(out[0].passed, Some(true));
        assert_eq!(out[1].passed, Some(false), "drawdown over the bar");
        assert_eq!(out[2].passed, None, "a missing metric is shown as missing");
        assert!(!all_pass(&out));
        assert!(
            evaluate("free text", m.as_object().unwrap()).is_none(),
            "not checkable"
        );
        assert!(evaluate(r#"{"checks": [{"metric": "x"}]}"#, m.as_object().unwrap()).is_none());
        assert!(!all_pass(&[]), "no criteria is never a pass");
    }

    #[test]
    fn bodies_are_checked_and_hashed_the_same_way_every_time() {
        let a = json!({"metrics": {"b": 1.0, "a": 2.0}, "trial": 9});
        let b: Value =
            serde_json::from_str(r#"{"trial": 9, "metrics": {"a": 2.0, "b": 1.0}}"#).unwrap();
        assert_eq!(hash(&a), hash(&b), "key order doesn't change the hash");
        assert!(validate(&a).is_ok());
        assert!(validate(&json!({"metrics": {}})).is_err());
        assert!(
            validate(&json!({"metrics": {"x": "high"}})).is_err(),
            "numbers only"
        );
        assert!(validate(&json!([1, 2])).is_err());
        let big = json!({"metrics": {"x": 1.0}, "pad": "x".repeat(MAX_BODY)});
        assert!(validate(&big).is_err());
    }
}
