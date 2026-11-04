//! Roles and what each may do (DESIGN §4c). The table is the whole policy: anything not listed is
//! refused. An agent's role comes from the store, never from its own request.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The owner. Human-only actions also need a `HumanProof`.
    Board,
    HeadOfDesk,
    Researcher,
    Strategist,
    Developer,
    PlatformEngineer,
    Validator,
    StressLead,
    RiskManager,
    Compliance,
    Ops,
    ProductOwner,
}

pub const ROLES: [Role; 12] = [
    Role::Board,
    Role::HeadOfDesk,
    Role::Researcher,
    Role::Strategist,
    Role::Developer,
    Role::PlatformEngineer,
    Role::Validator,
    Role::StressLead,
    Role::RiskManager,
    Role::Compliance,
    Role::Ops,
    Role::ProductOwner,
];

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Board => "board",
            Role::HeadOfDesk => "head_of_desk",
            Role::Researcher => "researcher",
            Role::Strategist => "strategist",
            Role::Developer => "developer",
            Role::PlatformEngineer => "platform_engineer",
            Role::Validator => "validator",
            Role::StressLead => "stress_lead",
            Role::RiskManager => "risk_manager",
            Role::Compliance => "compliance",
            Role::Ops => "ops",
            Role::ProductOwner => "product_owner",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        ROLES.into_iter().find(|r| r.as_str() == s)
    }

    /// The second line reports to the Board, not to the Head of Desk.
    pub fn second_line(self) -> bool {
        matches!(
            self,
            Role::Validator | Role::StressLead | Role::RiskManager | Role::Compliance
        )
    }
}

/// Everything an agent can ask the desk to do, apart from signing a lifecycle step
/// (`lifecycle::required`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    CreateTicket,
    CreateHypothesis,
    CreateStrategy,
    EditSpec,
    SubmitStrategyVersion,
    SubmitPlatformChange,
    RunDevBacktest,
    RunOfficialBacktest,
    RunStress,
    ReadResearchData,
    ReadHeldOut,
    ReadAccount,
    ReadConversations,
    ProposeLimits,
    TightenLimit,
    /// Raising or loosening a limit: the Board, with a `HumanProof` (S30).
    LoosenLimit,
    FileReport,
    Halt,
}

/// The policy table.
pub fn allowed(role: Role, action: Action) -> bool {
    use Action::*;
    use Role::*;
    match action {
        // anyone may raise a ticket or stop things; only adding risk is restricted
        CreateTicket | Halt => true,
        CreateHypothesis => matches!(role, Researcher),
        CreateStrategy | EditSpec => matches!(role, Strategist),
        SubmitStrategyVersion => matches!(role, Developer),
        SubmitPlatformChange => matches!(role, PlatformEngineer),
        RunDevBacktest => matches!(role, Researcher | Developer),
        RunOfficialBacktest => matches!(role, Validator),
        RunStress => matches!(role, StressLead),
        ReadResearchData => matches!(
            role,
            Researcher | Strategist | Developer | Validator | StressLead | Board
        ),
        // held-out rows only to the second line that judges with them (S33)
        ReadHeldOut => matches!(role, Validator | StressLead),
        ReadAccount => matches!(role, RiskManager | Ops | Compliance | ProductOwner | Board),
        ReadConversations => matches!(role, Board),
        ProposeLimits => matches!(role, RiskManager),
        TightenLimit => matches!(role, RiskManager | Board),
        LoosenLimit => matches!(role, Board),
        FileReport => !matches!(role, Researcher | Developer | PlatformEngineer | Strategist),
    }
}

/// Proof that the owner, not an agent, is acting. Only the Touch ID / password Keychain prompt can
/// make one (S30); an agent token never carries one.
#[derive(Debug)]
pub struct HumanProof(());

impl HumanProof {
    /// shortcut: the Keychain prompt arrives with the Desk API (build step 2); until then no code
    /// path outside tests can prove a human, so human-only actions are refused (fail closed).
    pub fn from_keychain_prompt() -> crate::Result<HumanProof> {
        Err(crate::DeskError::Denied(
            "the Touch ID signing prompt isn't wired yet (build step 2)".into(),
        ))
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> HumanProof {
        HumanProof(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_and_the_policy_matches_the_charters() {
        for r in ROLES {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("root"), None);
        // the Developer can't read held-out data (spec §11)
        assert!(!allowed(Role::Developer, Action::ReadHeldOut));
        assert!(allowed(Role::Validator, Action::ReadHeldOut));
        assert!(allowed(Role::StressLead, Action::ReadHeldOut));
        for r in ROLES {
            assert!(allowed(r, Action::Halt), "{r:?} may halt");
            if !matches!(r, Role::Validator | Role::StressLead) {
                assert!(!allowed(r, Action::ReadHeldOut), "{r:?} sees held-out");
            }
            if r != Role::Board {
                assert!(!allowed(r, Action::LoosenLimit), "{r:?} loosens a limit");
                assert!(!allowed(r, Action::ReadConversations));
            }
        }
        assert!(!allowed(Role::Researcher, Action::ReadAccount));
        assert!(!allowed(Role::Developer, Action::RunOfficialBacktest));
        assert!(!allowed(Role::Validator, Action::SubmitStrategyVersion));
        assert!(Role::Validator.second_line() && !Role::HeadOfDesk.second_line());
        assert!(HumanProof::from_keychain_prompt().is_err());
    }
}
