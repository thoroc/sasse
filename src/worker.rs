//! One pass of the queue.
//!
//! A tick takes the lease, advances the queue by at most one candidate, and
//! gives the lease back. It is the whole of the queue's behaviour: `sasse work`
//! is only a loop around it. Keeping it a single function of persisted state,
//! rather than a resident loop with state in memory, is what makes the
//! behaviour testable and what lets a crashed worker be resumed by the next
//! tick rather than needing recovery logic of its own.

use std::path::PathBuf;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use rusqlite::Connection;

use crate::config::{self, Config};
use crate::gate::{self, Gate};
use crate::git::{self, Git, MergeOutcome, RefUpdate, Sha};
use crate::queue::model::Verdict as Bisection;
use crate::queue::store::{self, Blame, Candidate};
use crate::queue::{Acquisition, CandidateState, EntryId, bisect, lease};

/// Why a candidate was abandoned without blaming anything in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abandoned {
    /// The base branch moved, so what the gate proved no longer applies.
    BaseMoved,
}

/// Why a candidate was pinned on one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// It would not merge onto the base at all.
    Conflict,
    /// It was alone in the candidate and the gate failed.
    GateFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// Nothing is queued.
    Idle,
    /// Another worker has the branch. Not an error.
    AnotherWorkerHolds { holder_pid: i32 },
    /// The base branch is checked out somewhere, so moving it would corrupt
    /// that checkout. Nothing was attempted.
    BaseCheckedOut { holders: Vec<PathBuf> },
    /// The candidate passed and the base now points at it.
    Landed {
        candidate: i64,
        entries: usize,
        at: Sha,
    },
    /// The candidate was thrown away and its entries went back in the queue,
    /// spending none of their retry budget.
    Abandoned {
        candidate: i64,
        requeued: usize,
        reason: Abandoned,
    },
    /// One entry was identified as the fault.
    Blamed {
        candidate: i64,
        entry: EntryId,
        branch: String,
        blame: Blame,
        reason: Fault,
    },
    /// The candidate failed with more than one entry in it, so it was split and
    /// the first half will be tried next.
    Split {
        failed: i64,
        next: i64,
        retrying: usize,
    },
}

pub struct Worker<'a> {
    conn: &'a mut Connection,
    git: &'a dyn Git,
    gate: &'a dyn Gate,
    repo_path: String,
    base_branch: String,
    log_dir: PathBuf,
    lease_ttl: Duration,
}

impl<'a> Worker<'a> {
    pub fn new(
        conn: &'a mut Connection,
        git: &'a dyn Git,
        gate: &'a dyn Gate,
        repo_path: impl Into<String>,
        base_branch: impl Into<String>,
        log_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            conn,
            git,
            gate,
            repo_path: repo_path.into(),
            base_branch: base_branch.into(),
            log_dir: log_dir.into(),
            lease_ttl: Duration::from_secs(300),
        }
    }

    pub fn with_lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl = ttl;
        self
    }

    /// Advance the queue by at most one candidate.
    pub fn tick(&mut self) -> Result<TickOutcome> {
        let lease = match lease::acquire(
            self.conn,
            &self.repo_path,
            &self.base_branch,
            self.lease_ttl,
        )? {
            Acquisition::Held { holder_pid, .. } => {
                return Ok(TickOutcome::AnotherWorkerHolds { holder_pid });
            }
            Acquisition::Acquired { lease, .. } => lease,
        };

        let outcome = self.advance();

        // Give the lease back whatever happened, so a tick that failed does not
        // hold the branch until the lease expires.
        let released = lease::release(self.conn, &lease);
        match outcome {
            Err(failed) => Err(failed),
            Ok(outcome) => {
                released.wrap_err("releasing the lease after the tick")?;
                Ok(outcome)
            }
        }
    }

    fn advance(&mut self) -> Result<TickOutcome> {
        let holders = git::checkouts_holding(self.git, &self.base_branch)?;
        if !holders.is_empty() {
            return Ok(TickOutcome::BaseCheckedOut { holders });
        }

        let base_sha = self.git.resolve(&self.base_branch)?;
        let config = self.read_config(&base_sha)?;

        let Some(candidate) = self.candidate_to_work_on(&base_sha, &config)? else {
            return Ok(TickOutcome::Idle);
        };

        // An unfinished candidate assembled against a base that has since moved
        // proves nothing about the base as it stands now.
        if candidate.base_sha != base_sha {
            return self.abandon(candidate.id, Abandoned::BaseMoved);
        }

        let candidate_sha = match self.assemble(&candidate, &config)? {
            Assembled::Candidate(sha) => sha,
            Assembled::Blamed(outcome) => return Ok(outcome),
        };

        store::mark_testing(self.conn, candidate.id, &candidate_sha)?;
        let verdict = self.gate_candidate(&candidate, &candidate_sha, &config)?;

        if verdict.passed() {
            self.land(&candidate, &candidate_sha)
        } else {
            self.on_gate_failure(&candidate, &config)
        }
    }

    /// The gate command, read out of the base commit rather than off disk. See
    /// `docs/adr/gate-provenance.md`.
    fn read_config(&self, base_sha: &Sha) -> Result<Config> {
        let source = self
            .git
            .read_file_at(base_sha, config::CONFIG_PATH)?
            .ok_or_else(|| {
                eyre!(
                    "{} is missing from {base_sha}, so there is no gate to run; \
                     the queue will not guess one",
                    config::CONFIG_PATH
                )
            })?;
        Config::parse(&source)
    }

    /// Resume the unfinished candidate if there is one, otherwise open a new one
    /// over whatever is queued.
    fn candidate_to_work_on(
        &mut self,
        base_sha: &Sha,
        config: &Config,
    ) -> Result<Option<Candidate>> {
        if let Some(active) =
            store::active_candidate(self.conn, &self.repo_path, &self.base_branch)?
        {
            return Ok(Some(active));
        }

        let batch = store::select_batch(
            self.conn,
            &self.repo_path,
            &self.base_branch,
            config.max_batch,
        )?;
        if batch.is_empty() {
            return Ok(None);
        }

        let id = store::open_candidate(
            self.conn,
            &self.repo_path,
            &self.base_branch,
            base_sha,
            &batch,
            None,
        )?;
        Ok(Some(Candidate {
            id,
            base_sha: base_sha.clone(),
            entries: batch,
        }))
    }

    fn assemble(&mut self, candidate: &Candidate, config: &Config) -> Result<Assembled> {
        self.git.checkout_detached(&candidate.base_sha)?;

        let mut tip = candidate.base_sha.clone();
        for entry in &candidate.entries {
            let message = format!("sasse candidate {}: {}", candidate.id, entry.branch);
            match self.git.merge(&entry.branch_sha, &message)? {
                MergeOutcome::Merged(sha) => tip = sha,
                // It cannot be integrated onto this base at all. Gating a batch
                // that will not assemble would tell us nothing.
                MergeOutcome::Conflicted => {
                    return Ok(Assembled::Blamed(self.blame(
                        candidate,
                        entry.id,
                        &entry.branch,
                        config.max_attempts,
                        Fault::Conflict,
                    )?));
                }
            }
        }

        Ok(Assembled::Candidate(tip))
    }

    fn gate_candidate(
        &mut self,
        candidate: &Candidate,
        candidate_sha: &Sha,
        config: &Config,
    ) -> Result<gate::Verdict> {
        let log_path = self.log_dir.join(format!("candidate-{}.log", candidate.id));
        let verdict = self.gate.run(&config.gate, candidate_sha, &log_path)?;

        let exit_code = match &verdict {
            gate::Verdict::Passed => Some(0),
            gate::Verdict::Failed { exit_code } => *exit_code,
        };
        store::record_run(
            self.conn,
            candidate.id,
            &config.gate,
            exit_code,
            &log_path.to_string_lossy(),
        )?;

        Ok(verdict)
    }

    /// Move the base onto the gated commit, but only if it has not moved since.
    fn land(&mut self, candidate: &Candidate, candidate_sha: &Sha) -> Result<TickOutcome> {
        match self
            .git
            .fast_forward(&self.base_branch, &candidate.base_sha, candidate_sha)?
        {
            RefUpdate::Updated => {
                let entries = store::land(self.conn, candidate.id)?;
                Ok(TickOutcome::Landed {
                    candidate: candidate.id,
                    entries,
                    at: candidate_sha.clone(),
                })
            }
            RefUpdate::Stale => self.abandon(candidate.id, Abandoned::BaseMoved),
        }
    }

    fn on_gate_failure(&mut self, candidate: &Candidate, config: &Config) -> Result<TickOutcome> {
        let ids: Vec<EntryId> = candidate.entries.iter().map(|e| e.id).collect();

        match bisect(&ids)? {
            // Alone in the candidate, so there is nothing left to narrow.
            Bisection::Culprit(entry) => {
                let branch = candidate
                    .entries
                    .iter()
                    .find(|e| e.id == entry)
                    .map(|e| e.branch.clone())
                    .unwrap_or_default();
                self.blame(
                    candidate,
                    entry,
                    &branch,
                    config.max_attempts,
                    Fault::GateFailed,
                )
            }
            // Someone in here is at fault but we do not know who. Halve it and
            // try the first half next; the rest goes back in the queue and will
            // be picked up once this half settles.
            Bisection::Bisect { left, .. } => {
                store::abandon(self.conn, candidate.id, CandidateState::Failed)?;
                let retry: Vec<_> = candidate
                    .entries
                    .iter()
                    .filter(|e| left.contains(&e.id))
                    .cloned()
                    .collect();
                let next = store::open_candidate(
                    self.conn,
                    &self.repo_path,
                    &self.base_branch,
                    &candidate.base_sha,
                    &retry,
                    Some(candidate.id),
                )?;
                Ok(TickOutcome::Split {
                    failed: candidate.id,
                    next,
                    retrying: retry.len(),
                })
            }
        }
    }

    fn blame(
        &mut self,
        candidate: &Candidate,
        entry: EntryId,
        branch: &str,
        max_attempts: u32,
        reason: Fault,
    ) -> Result<TickOutcome> {
        let description = match reason {
            Fault::Conflict => "conflicts with the base branch",
            Fault::GateFailed => "failed the gate on its own",
        };
        let blame = store::blame(self.conn, candidate.id, entry, max_attempts, description)?;
        Ok(TickOutcome::Blamed {
            candidate: candidate.id,
            entry,
            branch: branch.to_string(),
            blame,
            reason,
        })
    }

    fn abandon(&mut self, candidate: i64, reason: Abandoned) -> Result<TickOutcome> {
        let requeued = store::abandon(self.conn, candidate, CandidateState::Superseded)?;
        Ok(TickOutcome::Abandoned {
            candidate,
            requeued,
            reason,
        })
    }
}

enum Assembled {
    Candidate(Sha),
    Blamed(TickOutcome),
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::gate::fake::FakeGate;
    use crate::git::fake::FakeGit;
    use crate::queue::store::Entry;
    use crate::{db, queue};

    const REPO: &str = "/repo";
    const BASE: &str = "main";
    const CONFIG: &str = "gate = \"run-the-gate\"\n";

    fn base() -> Sha {
        FakeGit::commit(1)
    }

    /// A repository whose base branch is checked out nowhere and which has a
    /// gate configured in every commit.
    fn repo() -> FakeGit {
        FakeGit::new()
            .with_branch("refs/heads/main", &base())
            .with_file_everywhere(config::CONFIG_PATH, CONFIG)
            .with_checkout("/integration", None)
    }

    fn logs() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn queue_branch(conn: &Connection, name: &str, at: &Sha) -> EntryId {
        store::enqueue(conn, REPO, BASE, name, at).unwrap()
    }

    fn entry_state(conn: &Connection, id: EntryId) -> String {
        conn.query_row("SELECT state FROM entry WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn entry_attempts(conn: &Connection, id: EntryId) -> u32 {
        conn.query_row("SELECT attempts FROM entry WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn outcome_of(conn: &Connection, candidate: i64, entry: EntryId) -> String {
        conn.query_row(
            "SELECT outcome FROM candidate_entry WHERE candidate_id = ?1 AND entry_id = ?2",
            (candidate, entry),
            |r| r.get(0),
        )
        .unwrap()
    }

    fn candidate_state(conn: &Connection, id: i64) -> String {
        conn.query_row("SELECT state FROM candidate WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn an_empty_queue_does_nothing() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::passing();
        let dir = logs();

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        assert_eq!(outcome, TickOutcome::Idle);
        assert!(gate.calls().is_empty(), "nothing to gate");
    }

    #[test]
    fn a_branch_held_by_another_worker_is_left_alone() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        // Somebody else, alive and unexpired.
        let held = queue::lease::acquire(&mut conn, REPO, BASE, Duration::from_secs(600)).unwrap();
        assert!(matches!(held, Acquisition::Acquired { .. }));

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        assert!(
            matches!(outcome, TickOutcome::AnotherWorkerHolds { .. }),
            "got {outcome:?}"
        );
        assert!(gate.calls().is_empty());
    }

    /// Git will move a checked-out branch without complaint, so the tick must
    /// refuse before it does any work.
    #[test]
    fn a_checked_out_base_branch_stops_the_tick() {
        let mut conn = db::open_in_memory().unwrap();
        let git = FakeGit::new()
            .with_branch("refs/heads/main", &base())
            .with_file_everywhere(config::CONFIG_PATH, CONFIG)
            .with_checkout("/primary", Some("refs/heads/main"));
        let gate = FakeGate::passing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        match outcome {
            TickOutcome::BaseCheckedOut { holders } => {
                assert_eq!(holders, vec![PathBuf::from("/primary")])
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(entry_state(&conn, a), "queued", "nothing was touched");
        assert!(gate.calls().is_empty());
    }

    #[test]
    fn a_missing_gate_config_fails_the_tick_rather_than_guessing() {
        let mut conn = db::open_in_memory().unwrap();
        let git = FakeGit::new()
            .with_branch("refs/heads/main", &base())
            .with_checkout("/integration", None);
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let failed = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap_err();

        assert!(
            format!("{failed:?}").contains(config::CONFIG_PATH),
            "the error should name the missing file: {failed:?}"
        );
        assert!(gate.calls().is_empty());
    }

    /// A failed tick must not leave the branch locked until the lease expires.
    #[test]
    fn a_failing_tick_still_gives_the_lease_back() {
        let mut conn = db::open_in_memory().unwrap();
        let git = FakeGit::new()
            .with_branch("refs/heads/main", &base())
            .with_checkout("/integration", None);
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let _expected = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap_err();

        let held: i64 = conn
            .query_row("SELECT count(*) FROM worker_lease", [], |r| r.get(0))
            .unwrap();
        assert_eq!(held, 0, "the lease was not released");
    }

    #[test]
    fn a_clean_entry_that_passes_the_gate_lands() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::passing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        let (candidate, landed_at) = match outcome {
            TickOutcome::Landed {
                candidate,
                entries,
                at,
            } => {
                assert_eq!(entries, 1);
                (candidate, at)
            }
            other => panic!("expected a landing, got {other:?}"),
        };

        assert_eq!(entry_state(&conn, a), "merged");
        assert_eq!(outcome_of(&conn, candidate, a), "passed");
        assert_eq!(candidate_state(&conn, candidate), "passed");
        assert_eq!(
            git.resolve("refs/heads/main").unwrap(),
            landed_at,
            "the base must point at the commit that was gated"
        );
    }

    #[test]
    fn the_gate_command_comes_from_the_config() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        assert_eq!(gate.calls().len(), 1);
        assert_eq!(gate.calls()[0].0, "run-the-gate");
    }

    /// A branch changing sasse.toml does not get to choose the command that
    /// gates it. See docs/adr/gate-provenance.md.
    #[test]
    fn a_candidates_own_config_is_not_used_as_the_gate() {
        let mut conn = db::open_in_memory().unwrap();
        let hostile = FakeGit::commit(2);
        let git = repo().with_file(
            &hostile,
            config::CONFIG_PATH,
            "gate = \"something-nobody-reviewed\"\n",
        );
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &hostile);

        Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        assert_eq!(
            gate.calls()[0].0,
            "run-the-gate",
            "the gate must come from the base, not from the branch being gated"
        );
    }

    #[test]
    fn a_conflicting_entry_is_blamed_without_running_the_gate() {
        let mut conn = db::open_in_memory().unwrap();
        let bad = FakeGit::commit(9);
        let git = repo().with_conflict(&bad);
        let gate = FakeGate::passing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/bad", &bad);

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        match outcome {
            TickOutcome::Blamed {
                entry,
                blame,
                reason,
                ..
            } => {
                assert_eq!(entry, a);
                assert_eq!(reason, Fault::Conflict);
                assert_eq!(blame, Blame::Requeued, "it still has attempts left");
            }
            other => panic!("expected a blame, got {other:?}"),
        }
        assert!(
            gate.calls().is_empty(),
            "gating a batch that will not assemble proves nothing"
        );
        assert_eq!(entry_attempts(&conn, a), 1);
    }

    #[test]
    fn a_lone_entry_that_fails_the_gate_is_blamed() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::failing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        match outcome {
            TickOutcome::Blamed { entry, reason, .. } => {
                assert_eq!(entry, a);
                assert_eq!(reason, Fault::GateFailed);
            }
            other => panic!("expected a blame, got {other:?}"),
        }
        assert_eq!(entry_attempts(&conn, a), 1);
        assert_eq!(entry_state(&conn, a), "queued");
    }

    #[test]
    fn an_entry_is_evicted_once_its_retry_budget_is_spent() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo().with_file_everywhere(
            config::CONFIG_PATH,
            "gate = \"run-the-gate\"\nmax_attempts = 2\n",
        );
        let gate = FakeGate::failing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let first = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();
        assert!(matches!(
            first,
            TickOutcome::Blamed {
                blame: Blame::Requeued,
                ..
            }
        ));

        let second = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();
        assert!(
            matches!(
                second,
                TickOutcome::Blamed {
                    blame: Blame::Evicted,
                    ..
                }
            ),
            "got {second:?}"
        );
        assert_eq!(entry_state(&conn, a), "evicted");
        assert_eq!(entry_attempts(&conn, a), 2);
    }

    #[test]
    fn a_failed_batch_is_halved_and_the_rest_goes_back_in_the_queue() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::failing();
        let dir = logs();
        let ids: Vec<EntryId> = (2..6)
            .map(|n| queue_branch(&conn, &format!("feat/{n}"), &FakeGit::commit(n)))
            .collect();

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        let (failed, next) = match outcome {
            TickOutcome::Split {
                failed,
                next,
                retrying,
            } => {
                assert_eq!(retrying, 2, "four entries halve to two");
                (failed, next)
            }
            other => panic!("expected a split, got {other:?}"),
        };

        assert_eq!(candidate_state(&conn, failed), "failed");
        let retried = store::candidate_entries(&conn, next).unwrap();
        assert_eq!(
            retried.iter().map(|e| e.id).collect::<Vec<_>>(),
            ids[..2].to_vec(),
            "the first half, in merge order"
        );
        assert_eq!(entry_state(&conn, ids[2]), "queued");
        assert_eq!(entry_state(&conn, ids[3]), "queued");

        for id in &ids {
            assert_eq!(
                entry_attempts(&conn, *id),
                0,
                "a split blames nobody, so nobody pays"
            );
        }
    }

    #[test]
    fn the_batch_is_capped_by_the_config() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo().with_file_everywhere(
            config::CONFIG_PATH,
            "gate = \"run-the-gate\"\nmax_batch = 2\n",
        );
        let gate = FakeGate::passing();
        let dir = logs();
        for n in 2..7 {
            queue_branch(&conn, &format!("feat/{n}"), &FakeGit::commit(n));
        }

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        match outcome {
            TickOutcome::Landed { entries, .. } => assert_eq!(entries, 2),
            other => panic!("expected a landing, got {other:?}"),
        }
    }

    /// Invariant 2, end to end: the gate passed, but the base moved before the
    /// result could be used, so it is thrown away rather than forced.
    #[test]
    fn a_base_that_moves_during_the_gate_discards_the_candidate() {
        struct MovesTheBase<'g> {
            git: &'g FakeGit,
        }
        impl Gate for MovesTheBase<'_> {
            fn run(&self, _command: &str, _candidate: &Sha, _log: &Path) -> Result<gate::Verdict> {
                self.git
                    .force_branch("refs/heads/main", &FakeGit::commit(77));
                Ok(gate::Verdict::Passed)
            }
        }

        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = MovesTheBase { git: &git };
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        match outcome {
            TickOutcome::Abandoned {
                requeued, reason, ..
            } => {
                assert_eq!(requeued, 1);
                assert_eq!(reason, Abandoned::BaseMoved);
            }
            other => panic!("expected the candidate to be discarded, got {other:?}"),
        }
        assert_eq!(entry_state(&conn, a), "queued");
        assert_eq!(
            entry_attempts(&conn, a),
            0,
            "a moving base is nobody's fault"
        );
        assert_eq!(
            git.resolve("refs/heads/main").unwrap(),
            FakeGit::commit(77),
            "the base keeps whatever landed underneath us"
        );
    }

    #[test]
    fn an_unfinished_candidate_built_on_a_stale_base_is_discarded() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::passing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        // A candidate left over from a base that has since moved on.
        let stale = store::open_candidate(
            &mut conn,
            REPO,
            BASE,
            &FakeGit::commit(55),
            &[Entry {
                id: a,
                branch: "feat/a".into(),
                branch_sha: FakeGit::commit(2),
                attempts: 0,
            }],
            None,
        )
        .unwrap();

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        assert_eq!(
            outcome,
            TickOutcome::Abandoned {
                candidate: stale,
                requeued: 1,
                reason: Abandoned::BaseMoved,
            }
        );
        assert!(gate.calls().is_empty());
        assert_eq!(entry_attempts(&conn, a), 0);
    }

    /// The whole point of separating skipped from culprit: a neighbour's failure
    /// costs an entry nothing.
    #[test]
    fn entries_skipped_because_a_neighbour_conflicted_pay_nothing() {
        let mut conn = db::open_in_memory().unwrap();
        let bad = FakeGit::commit(9);
        let git = repo().with_conflict(&bad);
        let gate = FakeGate::passing();
        let dir = logs();
        let good = queue_branch(&conn, "feat/good", &FakeGit::commit(2));
        let culprit = queue_branch(&conn, "feat/bad", &bad);

        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        let candidate = match outcome {
            TickOutcome::Blamed { candidate, .. } => candidate,
            other => panic!("expected a blame, got {other:?}"),
        };

        assert_eq!(outcome_of(&conn, candidate, culprit), "culprit");
        assert_eq!(outcome_of(&conn, candidate, good), "skipped");
        assert_eq!(entry_attempts(&conn, culprit), 1);
        assert_eq!(entry_attempts(&conn, good), 0);
        assert_eq!(entry_state(&conn, good), "queued");
    }

    /// The cascade, driven to completion: four entries, one of which fails on
    /// its own. Bisection must land the three innocents and isolate the fourth.
    #[test]
    fn repeated_ticks_isolate_the_culprit_and_land_everyone_else() {
        use crate::gate::Verdict;

        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        // [a,b,c,d] fails; [a,b] passes and lands; [c,d] fails; [c] passes and
        // lands; [d] fails alone and is therefore the culprit.
        let gate = FakeGate::scripted([
            Verdict::Failed { exit_code: Some(1) },
            Verdict::Passed,
            Verdict::Failed { exit_code: Some(1) },
            Verdict::Passed,
            Verdict::Failed { exit_code: Some(1) },
        ]);
        let dir = logs();
        let ids: Vec<EntryId> = (2..6)
            .map(|n| queue_branch(&conn, &format!("feat/{n}"), &FakeGit::commit(n)))
            .collect();

        let mut outcomes = Vec::new();
        for _ in 0..5 {
            outcomes.push(
                Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
                    .tick()
                    .unwrap(),
            );
        }

        assert!(
            matches!(outcomes[0], TickOutcome::Split { retrying: 2, .. }),
            "tick 1 halves the batch: {:?}",
            outcomes[0]
        );
        assert!(
            matches!(outcomes[1], TickOutcome::Landed { entries: 2, .. }),
            "tick 2 lands the clean half: {:?}",
            outcomes[1]
        );
        assert!(
            matches!(outcomes[2], TickOutcome::Split { retrying: 1, .. }),
            "tick 3 halves the remaining pair: {:?}",
            outcomes[2]
        );
        assert!(
            matches!(outcomes[3], TickOutcome::Landed { entries: 1, .. }),
            "tick 4 lands the innocent one: {:?}",
            outcomes[3]
        );
        match &outcomes[4] {
            TickOutcome::Blamed { entry, reason, .. } => {
                assert_eq!(*entry, ids[3], "the last one standing is the culprit");
                assert_eq!(*reason, Fault::GateFailed);
            }
            other => panic!("tick 5 should isolate the culprit, got {other:?}"),
        }

        for innocent in &ids[..3] {
            assert_eq!(
                entry_state(&conn, *innocent),
                "merged",
                "entry {innocent} should have landed"
            );
            assert_eq!(
                entry_attempts(&conn, *innocent),
                0,
                "entry {innocent} was never at fault"
            );
        }
        assert_eq!(entry_attempts(&conn, ids[3]), 1);
    }

    #[test]
    fn every_gate_run_is_recorded_with_its_log() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .tick()
            .unwrap();

        let (command, exit, log): (String, i64, String) = conn
            .query_row(
                "SELECT command, exit_code, log_path FROM run ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(command, "run-the-gate");
        assert_eq!(exit, 0);
        assert!(log.ends_with(".log"), "the log path is recorded: {log}");
    }
}
