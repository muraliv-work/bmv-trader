//! The strategy lifecycle (DESIGN §4c): the only legal moves, who must sign each, and the
//! maker-checker rule. Pure; the store loads the facts (author, halt record, evidence) and calls
//! `check`. Halting is not a move here: it has its own path (`Store::halt`) so every halt is
//! recorded with its reason.

use crate::permissions::Role;
use crate::{DeskError, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idea,
    HypothesisApproved,
    SpecFrozen,
    Implemented,
    Validated,
    StressTested,
    LimitsSet,
    ComplianceCleared,
    PaperTrading,
    PaperReviewed,
    LiveApproved,
    Live,
    LiveFullSize,
    Halted,
    Retired,
}

/// The forward path, in order. Nothing may skip a step.
pub const PATH: [State; 13] = [
    State::Idea,
    State::HypothesisApproved,
    State::SpecFrozen,
    State::Implemented,
    State::Validated,
    State::StressTested,
    State::LimitsSet,
    State::ComplianceCleared,
    State::PaperTrading,
    State::PaperReviewed,
    State::LiveApproved,
    State::Live,
    State::LiveFullSize,
];

/// Paper trading needs at least this much before review.
pub const MIN_PAPER_DAYS: u32 = 20;
pub const MIN_PAPER_TRADES: u32 = 30;

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idea => "idea",
            State::HypothesisApproved => "hypothesis_approved",
            State::SpecFrozen => "spec_frozen",
            State::Implemented => "implemented",
            State::Validated => "validated",
            State::StressTested => "stress_tested",
            State::LimitsSet => "limits_set",
            State::ComplianceCleared => "compliance_cleared",
            State::PaperTrading => "paper_trading",
            State::PaperReviewed => "paper_reviewed",
            State::LiveApproved => "live_approved",
            State::Live => "live",
            State::LiveFullSize => "live_full_size",
            State::Halted => "halted",
            State::Retired => "retired",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        PATH.into_iter()
            .chain([State::Halted, State::Retired])
            .find(|x| x.as_str() == s)
    }

    fn rank(self) -> Option<usize> {
        PATH.iter().position(|s| *s == self)
    }
}

/// Why a strategy was halted; it decides who may resume it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    RiskBreach,
    Compliance,
    Operational,
    Decay,
    Other,
}

impl HaltReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HaltReason::RiskBreach => "risk_breach",
            HaltReason::Compliance => "compliance",
            HaltReason::Operational => "operational",
            HaltReason::Decay => "decay",
            HaltReason::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<HaltReason> {
        [
            HaltReason::RiskBreach,
            HaltReason::Compliance,
            HaltReason::Operational,
            HaltReason::Decay,
            HaltReason::Other,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
    }

    /// Who may give this reason. A risk or compliance halt can only be lifted by the owner, so
    /// only the second line that owns it (or the owner) may call one; anyone may still halt for
    /// any other reason.
    pub fn may_call(self, role: Role) -> bool {
        match self {
            HaltReason::RiskBreach => matches!(role, Role::RiskManager | Role::Board),
            HaltReason::Compliance => matches!(role, Role::Compliance | Role::Board),
            HaltReason::Decay => {
                matches!(role, Role::ProductOwner | Role::RiskManager | Role::Board)
            }
            HaltReason::Operational | HaltReason::Other => true,
        }
    }
}

/// The kind of a legal move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One step along `PATH`.
    Forward,
    /// A failed gate, back to the spec or the implementation; the work must be redone.
    Back,
    /// Out of HALTED, to where it was halted from.
    Resume,
    Retire,
}

/// A legal move: its kind, who must sign (every role for Forward; any one for Back), and whether
/// the owner's Touch ID is required.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub kind: Kind,
    pub roles: &'static [Role],
    pub human: bool,
}

/// The rule for a move, or None if the move isn't legal. `halted` is the current halt record.
pub fn rule(from: State, to: State, halted: Option<(State, HaltReason)>) -> Option<Rule> {
    use Role::*;
    use State::*;
    let r = |kind, roles, human| Some(Rule { kind, roles, human });
    if to == Halted {
        return None; // only `Store::halt`, which records the reason
    }
    if to == Retired {
        return if from == Retired {
            None
        } else {
            r(Kind::Retire, &[Board], true)
        };
    }
    if from == Retired {
        return None;
    }
    if from == Halted {
        let (previous, reason) = halted?;
        if to != previous {
            return None;
        }
        return match reason {
            // a risk or compliance halt is lifted by the owner only
            HaltReason::RiskBreach | HaltReason::Compliance => r(Kind::Resume, &[Board], true),
            _ => r(Kind::Resume, &[Ops, Board], false),
        };
    }
    let (i, j) = (from.rank()?, to.rank()?);
    // a failed gate before live goes back; a live strategy is halted, never "failed" back
    if matches!(to, SpecFrozen | Implemented) && j < i && i < 10 {
        return r(
            Kind::Back,
            &[
                Validator,
                StressLead,
                RiskManager,
                Compliance,
                ProductOwner,
                Strategist,
            ],
            false,
        );
    }
    if j != i + 1 {
        return None;
    }
    match to {
        HypothesisApproved => r(Kind::Forward, &[Strategist], false),
        SpecFrozen => r(Kind::Forward, &[ProductOwner], false),
        Implemented => r(Kind::Forward, &[Strategist], false),
        Validated => r(Kind::Forward, &[Validator], false),
        StressTested => r(Kind::Forward, &[StressLead], false),
        LimitsSet => r(Kind::Forward, &[Board], true),
        ComplianceCleared => r(Kind::Forward, &[Compliance], false),
        // shortcut: "automatic" in the spec; Ops starts it once every gate is signed
        PaperTrading => r(Kind::Forward, &[Ops], false),
        PaperReviewed => r(Kind::Forward, &[ProductOwner, RiskManager], false),
        LiveApproved => r(Kind::Forward, &[Board], true),
        Live => r(Kind::Forward, &[Ops], false),
        LiveFullSize => r(Kind::Forward, &[Board], true),
        _ => None,
    }
}

/// One signature on a move.
#[derive(Clone, Debug)]
pub struct Signature {
    pub agent_id: String,
    pub role: Role,
    /// True only when the Board signs with a `HumanProof`.
    pub human: bool,
}

/// Paper results, read from the store (never from the caller), needed to leave PAPER_TRADING.
#[derive(Clone, Copy, Debug)]
pub struct PaperEvidence {
    pub days: u32,
    pub trades: u32,
}

/// Checks a move: it's legal, every required role signed, no signer is the author (by agent or by
/// role), human steps carry the owner's signature, and paper review has its minimum evidence.
pub fn check(
    from: State,
    to: State,
    halted: Option<(State, HaltReason)>,
    author: (&str, Role),
    signatures: &[Signature],
    paper: Option<PaperEvidence>,
) -> Result<Rule> {
    let Some(rule) = rule(from, to, halted) else {
        return Err(DeskError::Illegal(format!(
            "{} -> {}",
            from.as_str(),
            to.as_str()
        )));
    };
    if signatures.is_empty() {
        return Err(DeskError::Denied("no signature".into()));
    }
    let mut seen = std::collections::HashSet::new();
    for s in signatures {
        if !seen.insert(s.agent_id.as_str()) {
            return Err(DeskError::Denied("one agent can't sign twice".into()));
        }
        // resuming and retiring judge no artifact, so there is no author to check against
        if matches!(rule.kind, Kind::Forward | Kind::Back)
            && (s.agent_id == author.0 || s.role == author.1)
        {
            return Err(DeskError::Denied(format!(
                "maker-checker: {} ({}) can't sign work authored by {} ({})",
                s.agent_id,
                s.role.as_str(),
                author.0,
                author.1.as_str()
            )));
        }
        if s.role == Role::Board && !s.human {
            return Err(DeskError::Denied(
                "a Board signature needs the owner's Touch ID".into(),
            ));
        }
    }
    let covered = match rule.kind {
        Kind::Forward => rule
            .roles
            .iter()
            .all(|r| signatures.iter().any(|s| s.role == *r)),
        _ => signatures.iter().any(|s| rule.roles.contains(&s.role)),
    };
    if !covered {
        return Err(DeskError::Unsigned(rule.roles.to_vec()));
    }
    if rule.human && !signatures.iter().any(|s| s.human && s.role == Role::Board) {
        return Err(DeskError::Denied(format!(
            "{} is the owner's decision (Touch ID)",
            to.as_str()
        )));
    }
    if to == State::PaperReviewed {
        match paper {
            Some(p) if p.days >= MIN_PAPER_DAYS && p.trades >= MIN_PAPER_TRADES => {}
            _ => {
                return Err(DeskError::Denied(format!(
                    "paper review needs at least {MIN_PAPER_DAYS} trading days and {MIN_PAPER_TRADES} trades"
                )));
            }
        }
    }
    Ok(rule)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(id: &str, role: Role, human: bool) -> Signature {
        Signature {
            agent_id: id.into(),
            role,
            human,
        }
    }
    const DEV: (&str, Role) = ("dev-1", Role::Developer);

    fn denied_with(r: Result<Rule>, text: &str) -> bool {
        matches!(r, Err(ref e @ (DeskError::Denied(_) | DeskError::Unsigned(_))) if e.to_string().contains(text))
    }

    #[test]
    fn states_round_trip_and_nothing_skips_a_step() {
        for s in PATH.into_iter().chain([State::Halted, State::Retired]) {
            assert_eq!(State::parse(s.as_str()), Some(s));
        }
        for (i, from) in PATH.iter().enumerate() {
            for (j, to) in PATH.iter().enumerate() {
                let back = matches!(to, State::SpecFrozen | State::Implemented) && j < i && i < 10;
                assert_eq!(
                    rule(*from, *to, None).is_some(),
                    j == i + 1 || back,
                    "{from:?} -> {to:?}"
                );
            }
            assert!(
                rule(*from, State::Halted, None).is_none(),
                "halting is Store::halt's"
            );
        }
        let skip = check(
            State::Implemented,
            State::StressTested,
            None,
            DEV,
            &[sig("st", Role::StressLead, false)],
            None,
        );
        assert!(matches!(skip, Err(DeskError::Illegal(_))));
        assert!(rule(State::Retired, State::Idea, None).is_none());
    }

    #[test]
    fn maker_checker_refuses_the_author_by_agent_and_by_role() {
        // the Strategist reviews code; a Strategist who also wrote it is refused for that reason
        let author = ("strat-1", Role::Strategist);
        let me = [sig("strat-1", Role::Strategist, false)];
        assert!(denied_with(
            check(
                State::SpecFrozen,
                State::Implemented,
                None,
                author,
                &me,
                None
            ),
            "maker-checker"
        ));
        let peer = [sig("strat-2", Role::Strategist, false)];
        assert!(denied_with(
            check(
                State::SpecFrozen,
                State::Implemented,
                None,
                author,
                &peer,
                None
            ),
            "maker-checker"
        ));
        // a two-signer step fails if either signer is the author
        let paper = Some(PaperEvidence {
            days: 25,
            trades: 40,
        });
        let po_author = ("po-1", Role::ProductOwner);
        let both = [
            sig("po-1", Role::ProductOwner, false),
            sig("risk-1", Role::RiskManager, false),
        ];
        assert!(denied_with(
            check(
                State::PaperTrading,
                State::PaperReviewed,
                None,
                po_author,
                &both,
                paper
            ),
            "maker-checker"
        ));
        let ok = [sig("val-1", Role::Validator, false)];
        assert!(check(State::Implemented, State::Validated, None, DEV, &ok, None).is_ok());
        let twice = [
            sig("val-1", Role::Validator, false),
            sig("val-1", Role::Validator, false),
        ];
        assert!(denied_with(
            check(
                State::Implemented,
                State::Validated,
                None,
                DEV,
                &twice,
                None
            ),
            "twice"
        ));
    }

    #[test]
    fn live_and_limits_need_the_owner() {
        let agent_board = [sig("board", Role::Board, false)];
        let owner = [sig("board", Role::Board, true)];
        let paper = Some(PaperEvidence {
            days: 25,
            trades: 40,
        });
        assert!(denied_with(
            check(
                State::PaperReviewed,
                State::LiveApproved,
                None,
                DEV,
                &agent_board,
                None
            ),
            "Touch ID"
        ));
        assert!(denied_with(
            check(
                State::PaperReviewed,
                State::LiveApproved,
                None,
                DEV,
                &[sig("risk", Role::RiskManager, false)],
                None
            ),
            "needs board"
        ));
        assert!(
            check(
                State::PaperReviewed,
                State::LiveApproved,
                None,
                DEV,
                &owner,
                None
            )
            .is_ok()
        );
        assert!(
            check(
                State::StressTested,
                State::LimitsSet,
                None,
                DEV,
                &agent_board,
                None
            )
            .is_err()
        );
        assert!(check(State::Live, State::LiveFullSize, None, DEV, &owner, None).is_ok());
        let both = [
            sig("po", Role::ProductOwner, false),
            sig("risk", Role::RiskManager, false),
        ];
        assert!(denied_with(
            check(
                State::PaperTrading,
                State::PaperReviewed,
                None,
                DEV,
                &both[..1],
                paper
            ),
            "needs"
        ));
        assert!(denied_with(
            check(
                State::PaperTrading,
                State::PaperReviewed,
                None,
                DEV,
                &both,
                Some(PaperEvidence {
                    days: 19,
                    trades: 40
                })
            ),
            "paper review"
        ));
        assert!(denied_with(
            check(
                State::PaperTrading,
                State::PaperReviewed,
                None,
                DEV,
                &both,
                Some(PaperEvidence {
                    days: 20,
                    trades: 29
                })
            ),
            "paper review"
        ));
        assert!(
            check(
                State::PaperTrading,
                State::PaperReviewed,
                None,
                DEV,
                &both,
                paper
            )
            .is_ok()
        );
    }

    #[test]
    fn only_the_owner_lifts_a_risk_halt_and_reasons_are_owned() {
        let risk = Some((State::Live, HaltReason::RiskBreach));
        let ops = [sig("ops-1", Role::Ops, false)];
        assert!(check(State::Halted, State::Live, risk, DEV, &ops, None).is_err());
        assert!(
            check(
                State::Halted,
                State::Live,
                risk,
                DEV,
                &[sig("board", Role::Board, true)],
                None
            )
            .is_ok()
        );
        let op = Some((State::Live, HaltReason::Operational));
        assert!(check(State::Halted, State::Live, op, DEV, &ops, None).is_ok());
        assert!(
            check(State::Halted, State::LiveFullSize, op, DEV, &ops, None).is_err(),
            "not where it was"
        );
        assert!(
            check(State::Halted, State::Live, None, DEV, &ops, None).is_err(),
            "no halt record"
        );
        assert!(
            check(
                State::Validated,
                State::Implemented,
                None,
                DEV,
                &[sig("st", Role::StressLead, false)],
                None
            )
            .is_ok()
        );
        assert!(
            check(
                State::Live,
                State::Implemented,
                None,
                DEV,
                &[sig("st", Role::StressLead, false)],
                None
            )
            .is_err()
        );
        assert!(
            check(
                State::Live,
                State::Retired,
                None,
                DEV,
                &[sig("board", Role::Board, true)],
                None
            )
            .is_ok()
        );
        assert!(
            HaltReason::RiskBreach.may_call(Role::RiskManager)
                && !HaltReason::RiskBreach.may_call(Role::Ops)
        );
        assert!(!HaltReason::Compliance.may_call(Role::Developer));
        assert!(HaltReason::Other.may_call(Role::Researcher));
    }
}
