//! What Claude may say about a candidate (DESIGN §3, §8). There is no field for symbol, side,
//! prices or quantity, so the type system leaves Claude only one power: shrink or veto.
//! Parsing Claude's JSON into a Verdict arrives in P4; until then the engine uses `Approve`.

/// A size reduction to 1–99% of what the rules allow (100% is `Approve`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Factor(u8);

impl Factor {
    pub fn percent(pct: u8) -> Option<Self> {
        (1..=99).contains(&pct).then_some(Factor(pct))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Reduce(Factor),
    Reject,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_is_strictly_between_0_and_100() {
        assert_eq!(Factor::percent(0), None);
        assert_eq!(Factor::percent(100), None);
        assert_eq!(Factor::percent(255), None);
        assert_eq!(Factor::percent(1).map(Factor::get), Some(1));
        assert_eq!(Factor::percent(99).map(Factor::get), Some(99));
    }
}
