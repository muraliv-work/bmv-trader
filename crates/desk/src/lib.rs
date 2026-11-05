//! The Quant Desk control plane (DESIGN §4c, DECISION S30): who may do what, the strategy
//! lifecycle, the hash-chained audit log, and the store that enforces them. Agents act only through
//! this; Paperclip schedules them but can't open a gate. Nothing here places an order.

pub mod agent;
pub mod api;
pub mod audit;
pub mod evidence;
pub mod lifecycle;
pub mod mcp;
pub mod owner;
pub mod permissions;
pub mod store;

/// Why the desk refused something. Every refusal is final for that request (fail closed).
/// Refusals are returned to the caller, not written to the audit chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DeskError {
    #[error("not permitted: {0}")]
    Denied(String),
    /// A lifecycle step that still lacks these signing roles.
    #[error("not permitted: needs {}", roles(.0))]
    Unsigned(Vec<permissions::Role>),
    #[error("not a legal transition: {0}")]
    Illegal(String),
    #[error("budget: {0}")]
    Budget(String),
    #[error("audit chain broken at entry {0}")]
    Tampered(i64),
    #[error("not found: {0}")]
    NotFound(String),
    /// A stored value that can't be read back: treat the database as damaged.
    #[error("corrupt store: {0}")]
    Corrupt(String),
    #[error("store: {0}")]
    Db(#[from] sqlx::Error),
    /// The machine failed under the desk (randomness, files): not the caller's fault.
    #[error("internal: {0}")]
    Internal(String),
}

fn roles(r: &[permissions::Role]) -> String {
    r.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(" + ")
}

pub type Result<T> = std::result::Result<T, DeskError>;
