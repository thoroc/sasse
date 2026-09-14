use std::fmt;

use eyre::{Result, eyre};

/// What happened to one entry inside one candidate.
///
/// This is a property of the (entry, candidate) pair rather than of the entry,
/// because an entry joins several candidates over its life and earns a separate
/// verdict in each. Recording it on the entry's own state column would lose it
/// the moment the entry requeues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// In the candidate, no verdict yet.
    Pending,
    /// The candidate passed its gate with this entry in it.
    Passed,
    /// Isolated by bisection as the entry that failed its own gate.
    Culprit,
    /// Never gated on its own, because a batch-mate was the culprit.
    Skipped,
}

impl Outcome {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }

    /// Whether this outcome spends one of the entry's retry attempts.
    ///
    /// Only the culprit pays. An entry skipped because a batch-mate failed
    /// requeues for free, otherwise a single flaky branch drains the retry
    /// budget of every branch that happened to be batched alongside it.
    pub fn burns_attempt(self) -> bool {
        matches!(self, Self::Culprit)
    }

    /// Whether the entry goes back in the queue rather than settling.
    pub fn requeues(self) -> bool {
        matches!(self, Self::Skipped)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use Outcome::*;
        matches!(
            (self, next),
            (Pending, Passed) | (Pending, Culprit) | (Pending, Skipped)
        )
    }

    pub fn transition_to(self, next: Self) -> Result<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(eyre!("illegal outcome transition: {self} -> {next}"))
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Culprit => "culprit",
            Self::Skipped => "skipped",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Outcome; 4] = [
        Outcome::Pending,
        Outcome::Passed,
        Outcome::Culprit,
        Outcome::Skipped,
    ];

    #[test]
    fn terminal_outcomes_have_no_exits() {
        for from in ALL.iter().copied().filter(|o| o.is_terminal()) {
            for to in ALL {
                assert!(
                    !from.can_transition_to(to),
                    "{from} should be terminal but allows {to}"
                );
            }
        }
    }

    #[test]
    fn every_verdict_is_reachable_from_pending() {
        for to in ALL.iter().copied().filter(|o| o.is_terminal()) {
            assert!(
                Outcome::Pending.can_transition_to(to),
                "pending should reach {to}"
            );
        }
    }

    /// The whole reason the skipped and culprit cases are distinguished.
    #[test]
    fn only_the_culprit_spends_a_retry_attempt() {
        for o in ALL {
            assert_eq!(
                o.burns_attempt(),
                o == Outcome::Culprit,
                "{o} disagrees about spending an attempt"
            );
        }
    }

    #[test]
    fn a_skipped_entry_requeues_without_cost() {
        assert!(Outcome::Skipped.requeues());
        assert!(!Outcome::Skipped.burns_attempt());
    }

    #[test]
    fn a_settled_entry_does_not_requeue() {
        assert!(!Outcome::Passed.requeues());
        assert!(!Outcome::Culprit.requeues());
    }

    #[test]
    fn an_outcome_cannot_be_revised_once_reached() {
        assert!(Outcome::Skipped.transition_to(Outcome::Culprit).is_err());
    }
}
