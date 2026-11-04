//! The owner's approvals (DECISION S30): a human-only action waits as a pending request until the
//! owner signs its exact text with their Secure Enclave key, which needs Touch ID (`desk-sign`).
//! The desk keeps only the public key, so nothing on this Mac can forge an approval, and each
//! signature covers one action, one nonce and an expiry, so it can't be reused or replayed.

use crate::lifecycle::State;
use crate::permissions::{HumanProof, Role};
use crate::store::{Signer, Store, plain};
use crate::{DeskError, Result};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// How long a request waits for the owner's signature.
pub const REQUEST_MINUTES: i64 = 10;
/// What `init_owner` must be signed over, so the first key proves it can sign with Touch ID.
pub const INIT_TEXT: &str = "bmv-desk: register this Secure Enclave key as the owner (Board)";

/// Everything only the owner may do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerAction {
    AddAgent {
        id: String,
        role: Role,
    },
    /// A new API token for an agent, returned once to the owner who approved it.
    IssueToken {
        agent: String,
    },
    SetBudget {
        agent: String,
        limit_usd: String,
    },
    SetLimit {
        strategy: i64,
        name: String,
        value: String,
    },
    /// A human-only lifecycle step, pinned to the version the owner saw.
    Transition {
        strategy: i64,
        to: State,
        version: i64,
    },
}

impl OwnerAction {
    /// What the Touch ID dialog shows; also part of the signed text.
    pub fn describe(&self) -> String {
        match self {
            OwnerAction::AddAgent { id, role } => format!("Add agent {id} as {}", role.as_str()),
            OwnerAction::IssueToken { agent } => format!("Issue a new API token for {agent}"),
            OwnerAction::SetBudget { agent, limit_usd } => {
                format!("Set {agent}'s monthly budget to ${limit_usd}")
            }
            OwnerAction::SetLimit {
                strategy,
                name,
                value,
            } => {
                format!("Set limit {name} = {value} on strategy {strategy}")
            }
            OwnerAction::Transition {
                strategy,
                to,
                version,
            } => {
                format!(
                    "Move strategy {strategy} (version {version}) to {}",
                    to.as_str()
                )
            }
        }
    }
}

/// A request waiting for the owner.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub action: OwnerAction,
    pub requested_by: String,
    pub expires: String,
    /// The exact text to sign.
    pub text: String,
}

/// The text the owner signs for a request; the `desk` command rebuilds it to check what it shows.
pub fn signed_text(id: &str, action: &OwnerAction, expires: &str) -> String {
    format!(
        "bmv-desk owner approval\nrequest: {id}\naction: {}\ndetail: {}\nexpires: {expires}",
        action.describe(),
        serde_json::to_string(action).unwrap_or_default()
    )
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DeskError::Denied("not hex".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| DeskError::Denied("not hex".into()))
        })
        .collect()
}

/// A Secure Enclave P-256 public key as CryptoKit prints it: 64 bytes (x then y), in hex.
fn verifying_key(hex: &str) -> Result<VerifyingKey> {
    let raw = hex_decode(hex)?;
    if raw.len() != 64 {
        return Err(DeskError::Denied("an owner key is 64 bytes".into()));
    }
    let mut sec1 = vec![0x04];
    sec1.extend_from_slice(&raw);
    VerifyingKey::from_sec1_bytes(&sec1)
        .map_err(|_| DeskError::Denied("not a P-256 public key".into()))
}

/// Checks a raw 64-byte (r then s) ECDSA P-256 signature over SHA-256 of `text`, as CryptoKit makes.
pub fn verify(public_hex: &str, text: &str, signature_hex: &str) -> Result<()> {
    let key = verifying_key(public_hex)?;
    let sig = Signature::from_slice(&hex_decode(signature_hex)?)
        .map_err(|_| DeskError::Denied("not a P-256 signature".into()))?;
    key.verify(text.as_bytes(), &sig)
        .map_err(|_| DeskError::Denied("the owner's signature doesn't match".into()))
}

fn nonce() -> Result<String> {
    crate::store::random_hex(16)
}

impl Store {
    /// Registers the owner: their Secure Enclave public key and the Board agent `board`. Only on an
    /// empty desk, and only with a signature from that key over `INIT_TEXT` (so it needed Touch ID).
    /// Returns the Board's API token, shown once.
    pub async fn init_owner(&self, public_hex: &str, signature_hex: &str) -> Result<String> {
        verify(public_hex, INIT_TEXT, signature_hex)?;
        self.bootstrap_owner(public_hex).await
    }

    /// Anyone registered may ask the owner for a human-only action; it waits for their signature.
    pub async fn request_owner_action(&self, by: &str, action: OwnerAction) -> Result<Pending> {
        self.role(by).await?;
        // checked now, so the text the owner reads is plain and names things that exist
        match &action {
            OwnerAction::AddAgent { id, .. } => plain("agent id", id)?,
            OwnerAction::IssueToken { agent } | OwnerAction::SetBudget { agent, .. } => {
                plain("agent id", agent)?;
                self.role(agent).await?;
            }
            OwnerAction::SetLimit { strategy, name, .. } => {
                plain("limit name", name)?;
                self.state(*strategy).await?;
            }
            OwnerAction::Transition { strategy, .. } => {
                self.state(*strategy).await?;
            }
        }
        if let OwnerAction::SetBudget { limit_usd: v, .. }
        | OwnerAction::SetLimit { value: v, .. } = &action
        {
            Decimal::from_str(v).map_err(|_| DeskError::Denied(format!("not an amount: {v:?}")))?;
        }
        let id = nonce()?;
        let expires =
            (chrono::Utc::now() + chrono::Duration::minutes(REQUEST_MINUTES)).to_rfc3339();
        let text = signed_text(&id, &action, &expires);
        let payload =
            serde_json::to_string(&action).map_err(|e| DeskError::Denied(e.to_string()))?;
        self.insert_owner_request(by, &id, &payload, &expires)
            .await?;
        Ok(Pending {
            id,
            action,
            requested_by: by.into(),
            expires,
            text,
        })
    }

    pub async fn pending_owner_actions(&self) -> Result<Vec<Pending>> {
        let now = chrono::Utc::now().to_rfc3339();
        let rows = self.open_owner_requests(&now).await?;
        rows.into_iter()
            .map(|(id, payload, by, expires)| {
                let action: OwnerAction = serde_json::from_str(&payload)
                    .map_err(|e| DeskError::Corrupt(format!("owner request {id}: {e}")))?;
                let text = signed_text(&id, &action, &expires);
                Ok(Pending {
                    id,
                    action,
                    requested_by: by,
                    expires,
                    text,
                })
            })
            .collect()
    }

    /// The owner's signature over a pending request: checked, the request closed (so it can't be
    /// used twice), then the action run with a proof that exists only for it. Returns the new
    /// token when the action issued one.
    pub async fn approve_owner_action(
        &self,
        id: &str,
        signature_hex: &str,
    ) -> Result<Option<String>> {
        let payload = self
            .redeem_owner_request(id, |payload, expires, public| {
                let action: OwnerAction = serde_json::from_str(payload)
                    .map_err(|e| DeskError::Corrupt(format!("owner request {id}: {e}")))?;
                let when = chrono::DateTime::parse_from_rfc3339(expires)
                    .map_err(|e| DeskError::Corrupt(format!("owner request {id}: {e}")))?;
                if when < chrono::Utc::now() {
                    return Err(DeskError::Denied("this request expired; ask again".into()));
                }
                verify(public, &signed_text(id, &action, expires), signature_hex)
            })
            .await?;
        let action: OwnerAction = serde_json::from_str(&payload)
            .map_err(|e| DeskError::Corrupt(format!("owner request {id}: {e}")))?;
        let done = self.run_owner_action(action).await;
        if done.is_err() {
            self.reopen_owner_request(id).await?;
        }
        done
    }

    /// The approved action, with a proof that exists only for it.
    async fn run_owner_action(&self, action: OwnerAction) -> Result<Option<String>> {
        let proof = HumanProof::signed();
        let dec = |s: &str| {
            Decimal::from_str(s).map_err(|_| DeskError::Denied(format!("not an amount: {s:?}")))
        };
        let done = match action {
            OwnerAction::AddAgent { id, role } => {
                self.add_agent("board", &id, role, Some(proof)).await
            }
            OwnerAction::IssueToken { agent } => return self.issue_token(&agent).await.map(Some),
            OwnerAction::SetBudget { agent, limit_usd } => {
                self.set_budget("board", &agent, dec(&limit_usd)?, proof)
                    .await
            }
            OwnerAction::SetLimit {
                strategy,
                name,
                value,
            } => {
                self.set_limit("board", strategy, &name, dec(&value)?, Some(proof))
                    .await
            }
            OwnerAction::Transition {
                strategy,
                to,
                version,
            } => {
                if self.version(strategy).await? != version {
                    return Err(DeskError::Denied(
                        "the strategy changed since you were asked; ask again".into(),
                    ));
                }
                self.transition(
                    strategy,
                    to,
                    vec![Signer {
                        agent_id: "board".into(),
                        proof: Some(proof),
                    }],
                )
                .await
            }
        };
        done.map(|()| None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::SigningKey;
    use p256::ecdsa::signature::Signer as _;

    /// A software stand-in for the Secure Enclave key, printing as CryptoKit does.
    fn key() -> (SigningKey, String) {
        let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let point = sk.verifying_key().to_encoded_point(false);
        let hex = point.as_bytes()[1..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        (sk, hex)
    }

    fn sign(sk: &SigningKey, text: &str) -> String {
        let sig: Signature = sk.sign(text.as_bytes());
        sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    async fn desk() -> (Store, SigningKey) {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let (sk, public) = key();
        assert!(
            s.init_owner(&public, &sign(&sk, "something else"))
                .await
                .is_err(),
            "must sign INIT_TEXT"
        );
        s.init_owner(&public, &sign(&sk, INIT_TEXT)).await.unwrap();
        assert!(
            s.init_owner(&public, &sign(&sk, INIT_TEXT)).await.is_err(),
            "once"
        );
        (s, sk)
    }

    #[tokio::test]
    async fn tokens_rotate_requests_are_checked_and_a_failed_action_reopens() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let (sk, public) = key();
        let board = s.init_owner(&public, &sign(&sk, INIT_TEXT)).await.unwrap();
        assert_eq!(s.agent_for_token(&board).await.unwrap(), "board");
        let ask = |a| s.request_owner_action("board", a);

        // the text the owner reads is plain and names a real agent
        let hidden = OwnerAction::SetBudget {
            agent: "board\naction: nothing".into(),
            limit_usd: "1".into(),
        };
        assert!(ask(hidden).await.is_err());
        let ghost = OwnerAction::IssueToken {
            agent: "nobody".into(),
        };
        assert!(ask(ghost).await.is_err());
        let bad = OwnerAction::SetBudget {
            agent: "board".into(),
            limit_usd: "lots".into(),
        };
        assert!(ask(bad).await.is_err());

        // a reissued token replaces the old one
        let p = ask(OwnerAction::IssueToken {
            agent: "board".into(),
        })
        .await
        .unwrap();
        let fresh = s
            .approve_owner_action(&p.id, &sign(&sk, &p.text))
            .await
            .unwrap()
            .unwrap();
        assert!(
            s.agent_for_token(&board).await.is_err(),
            "the old token is revoked"
        );
        assert_eq!(s.agent_for_token(&fresh).await.unwrap(), "board");

        // an action that fails leaves the request open, so the owner's approval isn't spent
        let p = ask(OwnerAction::AddAgent {
            id: "board".into(),
            role: Role::Ops,
        })
        .await
        .unwrap();
        assert!(
            s.approve_owner_action(&p.id, &sign(&sk, &p.text))
                .await
                .is_err()
        );
        assert!(
            s.pending_owner_actions()
                .await
                .unwrap()
                .iter()
                .any(|q| q.id == p.id)
        );

        // one agent can't flood the owner's queue
        for i in 0..4 {
            ask(OwnerAction::AddAgent {
                id: format!("a{i}"),
                role: Role::Ops,
            })
            .await
            .unwrap();
        }
        assert!(
            ask(OwnerAction::AddAgent {
                id: "a9".into(),
                role: Role::Ops
            })
            .await
            .is_err()
        );
        s.verify_audit().await.unwrap();
    }

    #[tokio::test]
    async fn only_the_owners_signature_approves_and_only_once() {
        let (s, sk) = desk().await;
        let p = s
            .request_owner_action(
                "board",
                OwnerAction::AddAgent {
                    id: "risk-1".into(),
                    role: Role::RiskManager,
                },
            )
            .await
            .unwrap();
        assert_eq!(s.pending_owner_actions().await.unwrap().len(), 1);
        // a signature by another key, or over other text, is refused
        let other = SigningKey::from_slice(&[9u8; 32]).unwrap();
        assert!(
            s.approve_owner_action(&p.id, &sign(&other, &p.text))
                .await
                .is_err()
        );
        assert!(
            s.approve_owner_action(&p.id, &sign(&sk, "approve everything"))
                .await
                .is_err()
        );
        assert!(s.approve_owner_action(&p.id, "zz").await.is_err());
        s.approve_owner_action(&p.id, &sign(&sk, &p.text))
            .await
            .unwrap();
        assert_eq!(s.role("risk-1").await.unwrap(), Role::RiskManager);
        // the same signature can't be used again
        assert!(
            s.approve_owner_action(&p.id, &sign(&sk, &p.text))
                .await
                .is_err()
        );
        assert!(s.pending_owner_actions().await.unwrap().is_empty());
        // a signature for one request doesn't approve another
        let a = s
            .request_owner_action(
                "risk-1",
                OwnerAction::AddAgent {
                    id: "x".into(),
                    role: Role::Ops,
                },
            )
            .await
            .unwrap();
        let b = s
            .request_owner_action(
                "risk-1",
                OwnerAction::AddAgent {
                    id: "y".into(),
                    role: Role::Ops,
                },
            )
            .await
            .unwrap();
        assert!(
            s.approve_owner_action(&b.id, &sign(&sk, &a.text))
                .await
                .is_err()
        );
        assert!(
            s.request_owner_action(
                "ghost",
                OwnerAction::AddAgent {
                    id: "z".into(),
                    role: Role::Ops
                }
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn the_owner_approves_budgets_limits_and_live() {
        let (s, sk) = desk().await;
        let approve = |p: Pending| {
            let sig = sign(&sk, &p.text);
            let s = &s;
            async move { s.approve_owner_action(&p.id, &sig).await }
        };
        for (id, role) in [("res-1", Role::Researcher), ("risk-1", Role::RiskManager)] {
            let p = s
                .request_owner_action(
                    "board",
                    OwnerAction::AddAgent {
                        id: id.into(),
                        role,
                    },
                )
                .await
                .unwrap();
            approve(p).await.unwrap();
        }
        let p = s
            .request_owner_action(
                "board",
                OwnerAction::SetBudget {
                    agent: "risk-1".into(),
                    limit_usd: "5.00".into(),
                },
            )
            .await
            .unwrap();
        approve(p).await.unwrap();
        s.spend("risk-1", Decimal::from_str("4.00").unwrap())
            .await
            .unwrap();
        let strategy = s.create_strategy("res-1", "idea").await.unwrap();
        // the Risk Manager proposes a first limit; it takes effect only when the owner signs
        let p = s
            .request_owner_action(
                "risk-1",
                OwnerAction::SetLimit {
                    strategy,
                    name: "max_daily_loss_usd".into(),
                    value: "250".into(),
                },
            )
            .await
            .unwrap();
        assert!(p.text.contains("max_daily_loss_usd = 250"));
        approve(p).await.unwrap();
        // a lifecycle step pinned to a version that has moved on is refused
        let p = s
            .request_owner_action(
                "board",
                OwnerAction::Transition {
                    strategy,
                    to: State::Retired,
                    version: 7,
                },
            )
            .await
            .unwrap();
        assert!(matches!(approve(p).await, Err(DeskError::Denied(m)) if m.contains("changed")));
        let p = s
            .request_owner_action(
                "board",
                OwnerAction::Transition {
                    strategy,
                    to: State::Retired,
                    version: 0,
                },
            )
            .await
            .unwrap();
        approve(p).await.unwrap();
        assert_eq!(s.state(strategy).await.unwrap(), State::Retired);
        assert!(s.verify_audit().await.is_ok());
    }

    #[test]
    fn a_cryptokit_signature_verifies() {
        // made by CryptoKit (P256.Signing, the same formats as the Secure Enclave key in desk-sign)
        let public = "b9906187b86f5c5ea3c952445bdf7f2a77f0004d71d8e45548979bc1a84eb47a69c595f8569417c04ddc93c59d231ff85dc9fb152a65f7b5cd863502827fee3c";
        let sig = "ea35b50cc20506c6757b60434e7cf95b88ddda5db9f491d5738b1d3884c922a3d4a5ef5a836785f6d701d1cef71ad1158feb51b562184752b7bb044b390c134c";
        let text = "bmv-desk owner approval\nrequest: abc\naction: Add agent x as ops";
        verify(public, text, sig).unwrap();
        assert!(
            verify(
                public,
                "bmv-desk owner approval\nrequest: abd\naction: Add agent x as ops",
                sig
            )
            .is_err()
        );
    }

    #[test]
    fn requests_parse_strictly() {
        assert!(
            serde_json::from_str::<OwnerAction>(r#"{"kind":"add_agent","id":"x","role":"ops"}"#)
                .is_ok()
        );
        assert!(
            serde_json::from_str::<OwnerAction>(
                r#"{"kind":"add_agent","id":"x","role":"ops","extra":1}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<OwnerAction>(r#"{"kind":"place_order"}"#).is_err());
        assert!(verify("00", "t", "00").is_err());
    }
}
