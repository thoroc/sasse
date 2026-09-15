use std::fmt;

use eyre::{Result, eyre};

/// Lifecycle of a queued branch.
///
/// `Batched -> Queued` is the requeue edge: an entry ahead of this one in a
/// candidate failed, so this entry's result is void and it goes back in line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryState {
    Queued,
    Batched,
    Merged,
    Evicted,
}

/// Lifecycle of a batch under test.
///
/// `Superseded` covers a candidate abandoned before it produced a verdict,
/// which happens when the base branch moves underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateState {
    Building,
    Testing,
    Passed,
    Failed,
    Superseded,
}

impl EntryState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Evicted)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use EntryState::*;
        matches!(
            (self, next),
            (Queued, Batched)
                | (Queued, Evicted)
                | (Batched, Queued)
                | (Batched, Merged)
                | (Batched, Evicted)
        )
    }

    pub fn transition_to(self, next: Self) -> Result<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(eyre!("illegal entry transition: {self} -> {next}"))
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Batched => "batched",
            Self::Merged => "merged",
            Self::Evicted => "evicted",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "queued" => Ok(Self::Queued),
            "batched" => Ok(Self::Batched),
            "merged" => Ok(Self::Merged),
            "evicted" => Ok(Self::Evicted),
            other => Err(eyre!("not an entry state: {other:?}")),
        }
    }
}

impl CandidateState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Passed | Self::Failed | Self::Superseded)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use CandidateState::*;
        matches!(
            (self, next),
            (Building, Testing)
                | (Building, Superseded)
                | (Testing, Passed)
                | (Testing, Failed)
                | (Testing, Superseded)
        )
    }

    pub fn transition_to(self, next: Self) -> Result<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(eyre!("illegal candidate transition: {self} -> {next}"))
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Testing => "testing",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "building" => Ok(Self::Building),
            "testing" => Ok(Self::Testing),
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "superseded" => Ok(Self::Superseded),
            other => Err(eyre!("not a candidate state: {other:?}")),
        }
    }
}

impl fmt::Display for EntryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CandidateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRY_STATES: [EntryState; 4] = [
        EntryState::Queued,
        EntryState::Batched,
        EntryState::Merged,
        EntryState::Evicted,
    ];

    const CANDIDATE_STATES: [CandidateState; 5] = [
        CandidateState::Building,
        CandidateState::Testing,
        CandidateState::Passed,
        CandidateState::Failed,
        CandidateState::Superseded,
    ];

    #[test]
    fn entry_terminal_states_have_no_exits() {
        for from in ENTRY_STATES.iter().copied().filter(|s| s.is_terminal()) {
            for to in ENTRY_STATES {
                assert!(
                    !from.can_transition_to(to),
                    "{from} should be terminal but allows {to}"
                );
            }
        }
    }

    #[test]
    fn candidate_terminal_states_have_no_exits() {
        for from in CANDIDATE_STATES.iter().copied().filter(|s| s.is_terminal()) {
            for to in CANDIDATE_STATES {
                assert!(
                    !from.can_transition_to(to),
                    "{from} should be terminal but allows {to}"
                );
            }
        }
    }

    #[test]
    fn no_state_transitions_to_itself() {
        for s in ENTRY_STATES {
            assert!(!s.can_transition_to(s), "entry {s} self-transitions");
        }
        for s in CANDIDATE_STATES {
            assert!(!s.can_transition_to(s), "candidate {s} self-transitions");
        }
    }

    #[test]
    fn a_batched_entry_can_be_requeued_when_invalidated() {
        assert_eq!(
            EntryState::Batched
                .transition_to(EntryState::Queued)
                .unwrap(),
            EntryState::Queued
        );
    }

    #[test]
    fn an_entry_cannot_merge_without_being_batched() {
        assert!(
            EntryState::Queued
                .transition_to(EntryState::Merged)
                .is_err()
        );
    }

    /// Every state the database can hold must come back out as the same value,
    /// or a status listing would fail on a row the schema considers valid.
    #[test]
    fn every_entry_state_round_trips_through_its_text_form() {
        for state in ENTRY_STATES {
            assert_eq!(EntryState::parse(state.as_str()).unwrap(), state);
        }
    }

    #[test]
    fn every_candidate_state_round_trips_through_its_text_form() {
        for state in CANDIDATE_STATES {
            assert_eq!(CandidateState::parse(state.as_str()).unwrap(), state);
        }
    }

    #[test]
    fn an_unknown_state_is_rejected_rather_than_guessed() {
        assert!(EntryState::parse("nonsense").is_err());
        assert!(CandidateState::parse("nonsense").is_err());
    }

    #[test]
    fn a_candidate_cannot_pass_without_being_tested() {
        assert!(
            CandidateState::Building
                .transition_to(CandidateState::Passed)
                .is_err()
        );
    }
}
