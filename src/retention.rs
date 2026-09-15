//! Applying the log budget.
//!
//! The policy and the reasoning are in `docs/adr/log-retention.md`. This is the
//! part that joins the two halves: the database knows which candidate a log
//! belongs to and how its gate run ended, the filesystem knows how big the log
//! is, and [`crate::logs::plan`] decides what goes.

use eyre::{Result, WrapErr};
use rusqlite::Connection;

use crate::bytes::ByteSize;
use crate::logs::{self, Because, LogFile, Plan, RETAINED_TAIL_LINES};
use crate::queue::store;

/// Describe every log that may legally be pruned, and how big it is.
///
/// A row pointing at a file that is no longer there is reconciled rather than
/// reported: the row is updated to say the log has gone. That happens whenever
/// someone clears the log directory by hand, and leaving the row claiming a
/// file exists would make `sasse logs` fail on it forever.
pub fn survey(conn: &Connection, repo_path: &str, base_branch: &str) -> Result<Vec<LogFile>> {
    let mut found = Vec::new();

    for run in store::logged_runs(conn, repo_path, base_branch)? {
        let path = std::path::PathBuf::from(&run.log_path);

        match logs::size_of(&path) {
            Some(bytes) => found.push(LogFile {
                run_id: run.run_id,
                candidate_id: run.candidate_id,
                path,
                bytes,
                passed: run.passed,
            }),
            None => store::forget_log(conn, run.run_id, None)?,
        }
    }

    Ok(found)
}

/// What a prune did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub plan: Plan,
    pub tails_kept: usize,
}

/// Work out what would go, without touching anything.
pub fn dry_run(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    budget: ByteSize,
) -> Result<Plan> {
    Ok(logs::plan(survey(conn, repo_path, base_branch)?, budget))
}

/// Bring the log directory inside its budget.
pub fn apply(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    budget: ByteSize,
) -> Result<Pruned> {
    let plan = dry_run(conn, repo_path, base_branch, budget)?;
    let mut tails_kept = 0;

    for removal in &plan.remove {
        // A failure's last lines are worth keeping; a pass has nothing to
        // explain. Read before the file goes, obviously, and written to the row
        // before the file goes too, so an interruption between the two loses the
        // bytes rather than the reason.
        let tail = match removal.because {
            Because::NothingToExplain => None,
            Because::OverBudget => logs::tail_text(&removal.log.path, RETAINED_TAIL_LINES).ok(),
        };
        if tail.is_some() {
            tails_kept += 1;
        }

        store::forget_log(conn, removal.log.run_id, tail.as_deref())?;

        if let Err(undeleted) = std::fs::remove_file(&removal.log.path) {
            // The row already says the log has gone, which is the honest state:
            // nothing will offer this file again. Worth surfacing rather than
            // swallowing, because a directory that cannot be written to will
            // not stay inside any budget.
            return Err(undeleted)
                .wrap_err_with(|| format!("removing the gate log {}", removal.log.path.display()));
        }
    }

    Ok(Pruned { plan, tails_kept })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;
    use crate::db;
    use crate::git::Sha;
    use crate::queue::store::Entry;

    const REPO: &str = "/repo";
    const BASE: &str = "main";

    fn sha(seed: u8) -> Sha {
        Sha::parse(&format!("{seed:02x}").repeat(20)).unwrap()
    }

    struct Fixture {
        dir: TempDir,
        conn: Connection,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                conn: db::open_in_memory().unwrap(),
            }
        }

        /// A settled candidate with one gate run and a log of the given size.
        fn logged(
            &mut self,
            seed: u8,
            state: &str,
            exit_code: i32,
            bytes: usize,
        ) -> (i64, PathBuf) {
            let entry_id =
                store::enqueue(&self.conn, REPO, BASE, &format!("feat/{seed}"), &sha(seed))
                    .unwrap();
            let entry = Entry {
                id: entry_id,
                branch: format!("feat/{seed}"),
                branch_sha: sha(seed),
                attempts: 0,
            };
            let candidate = store::open_candidate(
                &mut self.conn,
                REPO,
                BASE,
                &sha(200),
                std::slice::from_ref(&entry),
                None,
            )
            .unwrap();

            let path = self.dir.path().join(format!("candidate-{candidate}.log"));
            let body: String = (0..bytes).map(|_| 'x').collect();
            std::fs::write(&path, body).unwrap();

            store::record_run(
                &self.conn,
                candidate,
                "gate",
                Some(exit_code),
                &path.to_string_lossy(),
            )
            .unwrap();
            // A gated or landed candidate has to name its commit, which the
            // schema enforces, so the fixture sets one alongside the state.
            self.conn
                .execute(
                    "UPDATE candidate SET state = ?2, candidate_sha = ?3 WHERE id = ?1",
                    (candidate, state, sha(seed).as_str()),
                )
                .unwrap();

            let run_id: i64 = self
                .conn
                .query_row(
                    "SELECT id FROM run WHERE candidate_id = ?1",
                    [candidate],
                    |r| r.get(0),
                )
                .unwrap();
            (run_id, path)
        }

        fn tail_of(&self, run_id: i64) -> Option<String> {
            self.conn
                .query_row("SELECT tail FROM run WHERE id = ?1", [run_id], |r| r.get(0))
                .unwrap()
        }

        fn path_of(&self, run_id: i64) -> Option<String> {
            self.conn
                .query_row("SELECT log_path FROM run WHERE id = ?1", [run_id], |r| {
                    r.get(0)
                })
                .unwrap()
        }
    }

    fn generous() -> ByteSize {
        ByteSize::new(10 * 1024 * 1024)
    }

    #[test]
    fn a_passing_log_is_removed_and_keeps_no_tail() {
        let mut f = Fixture::new();
        let (run, path) = f.logged(1, "passed", 0, 500);

        let pruned = apply(&f.conn, REPO, BASE, generous()).unwrap();

        assert_eq!(pruned.plan.remove.len(), 1);
        assert_eq!(pruned.tails_kept, 0, "a pass has nothing to explain");
        assert!(!path.exists());
        assert_eq!(f.path_of(run), None);
        assert_eq!(f.tail_of(run), None);
    }

    #[test]
    fn a_failing_log_survives_while_there_is_room() {
        let mut f = Fixture::new();
        let (run, path) = f.logged(1, "failed", 1, 500);

        let pruned = apply(&f.conn, REPO, BASE, generous()).unwrap();

        assert!(pruned.plan.is_empty());
        assert!(path.exists());
        assert!(f.path_of(run).is_some());
    }

    /// The point of the whole design: the bytes go, the reason stays.
    #[test]
    fn a_pruned_failure_leaves_its_reason_behind() {
        let mut f = Fixture::new();
        let (run, path) = f.logged(1, "failed", 1, 4096);
        std::fs::write(&path, "setting up\nrunning\nassertion failed: 1 != 2\n").unwrap();

        apply(&f.conn, REPO, BASE, ByteSize::new(1)).unwrap();

        assert!(!path.exists(), "the file should have gone");
        assert_eq!(f.path_of(run), None);
        let kept = f.tail_of(run).expect("a failure keeps its tail");
        assert!(
            kept.contains("assertion failed: 1 != 2"),
            "the reason should have survived: {kept:?}"
        );
    }

    #[test]
    fn a_candidate_still_being_gated_is_left_alone() {
        let mut f = Fixture::new();
        let (run, path) = f.logged(1, "testing", 1, 4096);

        let pruned = apply(&f.conn, REPO, BASE, ByteSize::new(1)).unwrap();

        assert!(pruned.plan.is_empty(), "nothing in flight may be pruned");
        assert!(path.exists());
        assert!(f.path_of(run).is_some());
    }

    #[test]
    fn passes_go_before_failures() {
        let mut f = Fixture::new();
        let (passing, passing_path) = f.logged(1, "passed", 0, 4000);
        let (failing, failing_path) = f.logged(2, "failed", 1, 100);

        apply(&f.conn, REPO, BASE, ByteSize::new(1000)).unwrap();

        assert!(!passing_path.exists(), "the pass should have gone");
        assert!(
            failing_path.exists(),
            "clearing the pass was enough, so the failure stays"
        );
        assert_eq!(f.path_of(passing), None);
        assert!(f.path_of(failing).is_some());
    }

    /// Someone clearing the log directory by hand must not leave rows pointing
    /// at files that are not there.
    #[test]
    fn a_row_pointing_at_a_vanished_log_is_reconciled() {
        let mut f = Fixture::new();
        let (run, path) = f.logged(1, "failed", 1, 500);
        std::fs::remove_file(&path).unwrap();

        let surveyed = survey(&f.conn, REPO, BASE).unwrap();

        assert!(surveyed.is_empty());
        assert_eq!(
            f.path_of(run),
            None,
            "the row should stop claiming a file exists"
        );
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let mut f = Fixture::new();
        let (run, path) = f.logged(1, "passed", 0, 500);

        let plan = dry_run(&f.conn, REPO, BASE, generous()).unwrap();

        assert_eq!(plan.remove.len(), 1, "it would remove the pass");
        assert!(path.exists(), "but it did not");
        assert!(f.path_of(run).is_some());
    }

    #[test]
    fn pruning_is_idempotent() {
        let mut f = Fixture::new();
        f.logged(1, "passed", 0, 500);

        let first = apply(&f.conn, REPO, BASE, generous()).unwrap();
        let second = apply(&f.conn, REPO, BASE, generous()).unwrap();

        assert_eq!(first.plan.remove.len(), 1);
        assert!(second.plan.is_empty(), "nothing left to do the second time");
    }

    #[test]
    fn the_budget_is_reached_across_several_failures() {
        let mut f = Fixture::new();
        let (oldest, oldest_path) = f.logged(1, "failed", 1, 1000);
        let (_middle, middle_path) = f.logged(2, "failed", 1, 1000);
        let (_newest, newest_path) = f.logged(3, "failed", 1, 1000);

        let pruned = apply(&f.conn, REPO, BASE, ByteSize::new(2500)).unwrap();

        assert_eq!(pruned.plan.bytes_before, 3000);
        assert!(pruned.plan.bytes_after <= 2500);
        assert!(!oldest_path.exists(), "the oldest failure goes first");
        assert!(middle_path.exists());
        assert!(newest_path.exists());
        assert_eq!(pruned.tails_kept, 1);
        assert!(f.tail_of(oldest).is_some());
    }

    #[test]
    fn logs_for_another_base_branch_are_not_surveyed() {
        let mut f = Fixture::new();
        f.logged(1, "passed", 0, 500);
        assert!(survey(&f.conn, REPO, "release").unwrap().is_empty());
    }

    #[test]
    fn a_missing_log_directory_is_not_an_error() {
        let f = Fixture::new();
        assert!(dry_run(&f.conn, REPO, BASE, generous()).unwrap().is_empty());
        let _ = Path::new("");
    }
}
