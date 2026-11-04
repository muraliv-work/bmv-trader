//! The hash-chained audit log (DESIGN §4c): each entry's hash covers the previous entry's hash, so
//! changing, removing or reordering any past entry breaks every later link. The store also refuses
//! UPDATE and DELETE on the table. `first_break` catches an edit made around the triggers, but not
//! a whole chain rewritten by someone who can write the file: that needs the head hash anchored
//! outside it (build step 2).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The hash before the first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: i64,
    pub at: String,
    pub agent_id: String,
    pub action: String,
    pub detail: String,
    pub prev_hash: String,
    pub hash: String,
}

/// The hash of an entry's content and its predecessor. Fields are length-prefixed, so no choice
/// of text can make two different entries hash alike by shifting a separator.
pub fn hash(
    prev_hash: &str,
    seq: i64,
    at: &str,
    agent_id: &str,
    action: &str,
    detail: &str,
) -> String {
    let mut h = Sha256::new();
    for part in [prev_hash, &seq.to_string(), at, agent_id, action, detail] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The next entry after `prev` (None: the first).
pub fn next(prev: Option<&Entry>, at: &str, agent_id: &str, action: &str, detail: &str) -> Entry {
    let (seq, prev_hash) = match prev {
        Some(p) => (p.seq + 1, p.hash.clone()),
        None => (1, GENESIS.to_string()),
    };
    Entry {
        hash: hash(&prev_hash, seq, at, agent_id, action, detail),
        seq,
        at: at.into(),
        agent_id: agent_id.into(),
        action: action.into(),
        detail: detail.into(),
        prev_hash,
    }
}

/// The first entry whose sequence, link or hash is wrong; None when the chain is whole.
pub fn first_break(entries: &[Entry]) -> Option<i64> {
    let mut prev_hash = GENESIS.to_string();
    for (i, e) in entries.iter().enumerate() {
        let want = hash(
            &e.prev_hash,
            e.seq,
            &e.at,
            &e.agent_id,
            &e.action,
            &e.detail,
        );
        if e.seq != i as i64 + 1 || e.prev_hash != prev_hash || e.hash != want {
            return Some(e.seq);
        }
        prev_hash = e.hash.clone();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(n: usize) -> Vec<Entry> {
        let mut out: Vec<Entry> = Vec::new();
        for i in 0..n {
            let e = next(
                out.last(),
                "2026-11-04T00:00:00Z",
                "ops-1",
                "halt",
                &format!("{{\"n\":{i}}}"),
            );
            out.push(e);
        }
        out
    }

    #[test]
    fn any_tamper_is_found() {
        let good = chain(5);
        assert_eq!(first_break(&good), None);
        let mut edited = good.clone();
        edited[2].detail = "{\"n\":99}".into();
        assert_eq!(first_break(&edited), Some(3));
        // re-hashing the edited entry still breaks the next link
        let mut rehashed = edited.clone();
        let e = &rehashed[2];
        rehashed[2].hash = hash(
            &e.prev_hash,
            e.seq,
            &e.at,
            &e.agent_id,
            &e.action,
            &e.detail,
        );
        assert_eq!(first_break(&rehashed), Some(4));
        let mut removed = good.clone();
        removed.remove(1);
        assert_eq!(first_break(&removed), Some(3));
        let mut swapped = good.clone();
        swapped.swap(1, 2);
        assert_eq!(first_break(&swapped), Some(3));
        // separators can't be shifted to forge a match
        assert_ne!(
            hash("a", 1, "b", "c", "d", "e"),
            hash("a", 1, "b", "cd", "", "e")
        );
    }
}
