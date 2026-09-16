//! Keeping gate logs inside a budget.
//!
//! The policy is in `docs/adr/002-log-retention.md`. In short: the log directory has
//! a hard ceiling, a passing candidate's log is removed as soon as it settles
//! because nobody reads a green gate log, and a failure's last lines are kept on
//! its run row so the reason survives the bytes.
//!
//! The decision about what goes is a pure function over descriptions of the
//! files, so it can be tested without a filesystem and reported as a dry run
//! without touching one.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use eyre::Result;

use crate::bytes::ByteSize;

/// How much of a failing log is kept on its row once the file has gone.
pub const RETAINED_TAIL_LINES: usize = 40;

/// A log the prune may consider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub run_id: i64,
    pub candidate_id: i64,
    pub path: PathBuf,
    pub bytes: u64,
    /// Whether the gate run this log belongs to passed.
    pub passed: bool,
}

/// Why a particular log was chosen for removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Because {
    /// It records a pass, so it has no readers.
    NothingToExplain,
    /// The directory was over budget and this was the oldest failure left.
    OverBudget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub log: LogFile,
    pub because: Because,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub remove: Vec<Removal>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub budget: ByteSize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty()
    }

    pub fn bytes_freed(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Decide which logs go.
///
/// Passing logs go regardless of the budget, because a green gate log has no
/// readers and keeping it spends allowance that a failure needs. Then the oldest
/// failures go, one at a time, only while the directory is still over budget.
///
/// Every failure's file may be removed, including the last one, because its last
/// lines are stored on its run row before the file goes. That is what makes the
/// budget a real ceiling rather than a suggestion: without the retained tail,
/// honouring it would mean discarding the reason something failed, and the
/// policy would have to soften. With it, nothing the design promised to keep is
/// lost.
///
/// Logs belonging to candidates still being assembled or gated are never
/// offered here, so everything this sees may legally go.
pub fn plan(logs: Vec<LogFile>, budget: ByteSize) -> Plan {
    let bytes_before: u64 = logs.iter().map(|log| log.bytes).sum();
    let mut remaining = bytes_before;
    let mut remove = Vec::new();

    let (passed, mut failed): (Vec<_>, Vec<_>) = logs.into_iter().partition(|log| log.passed);

    for log in passed {
        remaining = remaining.saturating_sub(log.bytes);
        remove.push(Removal {
            log,
            because: Because::NothingToExplain,
        });
    }

    // Oldest first: run ids are monotonic, so the id is the age.
    failed.sort_by_key(|log| log.run_id);
    for log in failed {
        if remaining <= budget.bytes() {
            break;
        }
        remaining = remaining.saturating_sub(log.bytes);
        remove.push(Removal {
            log,
            because: Because::OverBudget,
        });
    }

    Plan {
        remove,
        bytes_before,
        bytes_after: remaining,
        budget,
    }
}

/// The last `lines` lines of a file.
///
/// Reads forwards keeping a ring rather than seeking from the end, so a log far
/// larger than the part wanted is never held in memory. Lossy on invalid UTF-8:
/// gate output is whatever the gate printed, and refusing to show it because a
/// byte was not valid would be worse than showing it approximately.
pub fn tail(path: &Path, lines: usize) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let mut kept: VecDeque<String> = VecDeque::with_capacity(lines.saturating_add(1));

    for line in BufReader::new(file).split(b'\n') {
        let line = line?;
        kept.push_back(String::from_utf8_lossy(&line).into_owned());
        if kept.len() > lines {
            kept.pop_front();
        }
    }

    Ok(kept.into())
}

/// The last `lines` lines as one string, for keeping on a row.
pub fn tail_text(path: &Path, lines: usize) -> Result<String> {
    Ok(tail(path, lines)?.join("\n"))
}

/// How big a log is, or `None` if it is no longer there.
pub fn size_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(run_id: i64, bytes: u64, passed: bool) -> LogFile {
        LogFile {
            run_id,
            candidate_id: run_id,
            path: PathBuf::from(format!("/logs/candidate-{run_id}.log")),
            bytes,
            passed,
        }
    }

    fn removed(plan: &Plan) -> Vec<i64> {
        plan.remove.iter().map(|r| r.log.run_id).collect()
    }

    #[test]
    fn nothing_to_do_when_there_are_no_logs() {
        let plan = plan(Vec::new(), ByteSize::new(100));
        assert!(plan.is_empty());
        assert_eq!(plan.bytes_before, 0);
        assert_eq!(plan.bytes_after, 0);
    }

    /// A green gate log has no readers, so it goes whether or not the directory
    /// is anywhere near its budget.
    #[test]
    fn a_passing_log_goes_even_when_there_is_room_to_spare() {
        let plan = plan(vec![log(1, 10, true)], ByteSize::new(1_000_000));

        assert_eq!(removed(&plan), vec![1]);
        assert_eq!(plan.remove[0].because, Because::NothingToExplain);
        assert_eq!(plan.bytes_after, 0);
    }

    #[test]
    fn a_failing_log_stays_while_there_is_room() {
        let plan = plan(vec![log(1, 10, false)], ByteSize::new(1_000_000));
        assert!(plan.is_empty());
        assert_eq!(plan.bytes_after, 10);
    }

    #[test]
    fn failures_go_oldest_first_and_only_while_over_budget() {
        let plan = plan(
            vec![
                log(3, 100, false),
                log(1, 100, false),
                log(2, 100, false),
                log(4, 100, false),
            ],
            ByteSize::new(250),
        );

        assert_eq!(
            removed(&plan),
            vec![1, 2],
            "two of the four had to go, and they were the oldest two"
        );
        assert_eq!(plan.bytes_after, 200);
        assert!(plan.remove.iter().all(|r| r.because == Because::OverBudget));
    }

    /// Clearing the passes may be enough on its own, in which case no failure
    /// needs to be touched.
    #[test]
    fn clearing_the_passes_can_be_enough() {
        let plan = plan(
            vec![log(1, 900, true), log(2, 50, false), log(3, 50, false)],
            ByteSize::new(200),
        );

        assert_eq!(removed(&plan), vec![1]);
        assert_eq!(plan.bytes_after, 100);
    }

    #[test]
    fn the_plan_reports_the_size_before_and_after() {
        let plan = plan(
            vec![log(1, 500, true), log(2, 500, false)],
            ByteSize::new(400),
        );
        assert_eq!(plan.bytes_before, 1000);
        assert_eq!(plan.bytes_after, 0);
        assert_eq!(plan.bytes_freed(), 1000);
    }

    /// Removing the last failure's file is not losing the failure: its last
    /// lines are kept on the row before the file goes. That is precisely what
    /// lets the budget be a ceiling instead of a preference.
    #[test]
    fn the_last_failure_is_removed_rather_than_the_budget_being_broken() {
        let plan = plan(vec![log(1, 5000, false)], ByteSize::new(100));

        assert_eq!(removed(&plan), vec![1]);
        assert_eq!(plan.remove[0].because, Because::OverBudget);
        assert_eq!(plan.bytes_after, 0);
    }

    /// The whole promise of the chosen policy, asserted directly: whatever it
    /// is given, the plan brings the directory inside its budget.
    #[test]
    fn the_budget_is_always_met_whatever_the_mix() {
        let sizes = [1u64, 7, 100, 999, 4096];
        let budgets = [0u64, 1, 50, 300, 10_000];

        for budget in budgets {
            for pattern in 0..32u32 {
                let logs: Vec<LogFile> = sizes
                    .iter()
                    .enumerate()
                    .map(|(n, bytes)| log(n as i64 + 1, *bytes, pattern & (1 << n) != 0))
                    .collect();

                let outcome = plan(logs, ByteSize::new(budget));
                assert!(
                    outcome.bytes_after <= budget,
                    "budget {budget} with pattern {pattern:05b} left {} bytes",
                    outcome.bytes_after
                );
            }
        }
    }

    #[test]
    fn a_tail_is_read_without_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let body: String = (1..=5000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, body).unwrap();

        let last = tail(&path, 3).unwrap();
        assert_eq!(last, vec!["line 4998", "line 4999", "line 5000"]);
    }

    #[test]
    fn a_tail_of_a_short_file_is_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.log");
        std::fs::write(&path, "one\ntwo\n").unwrap();

        assert_eq!(tail(&path, 40).unwrap(), vec!["one", "two"]);
    }

    /// Gate output is whatever the gate printed, which may not be valid UTF-8.
    #[test]
    fn invalid_utf8_is_shown_approximately_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.log");
        std::fs::write(&path, [b'o', b'k', 0xff, b'\n']).unwrap();

        let last = tail(&path, 10).unwrap();
        assert_eq!(last.len(), 1);
        assert!(last[0].starts_with("ok"), "got {:?}", last[0]);
    }

    #[test]
    fn the_size_of_a_missing_log_is_nothing() {
        assert_eq!(size_of(Path::new("/definitely/not/here.log")), None);
    }
}
