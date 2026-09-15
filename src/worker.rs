//! One pass of the queue.
//!
//! A tick takes the lease, advances the queue by at most one candidate, and
//! gives the lease back. It is the whole of the queue's behaviour: `sasse work`
//! is only a loop around it. Keeping it a single function of persisted state,
//! rather than a resident loop with state in memory, is what makes the
//! behaviour testable and what lets a crashed worker be resumed by the next
//! tick rather than needing recovery logic of its own.

use std::path::{Path, PathBuf};
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
    /// The worker was asked to stop while the gate was running.
    ///
    /// A signal reaches the whole process group, so the gate's shell dies with
    /// the worker and exits non-zero. That is not the branch misbehaving, and
    /// charging it an attempt would let repeated interruptions evict something
    /// that never failed on its merits.
    Interrupted,
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

/// How the loop around `tick` is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkOptions {
    /// How long to wait when there is nothing to do, or when the queue needs a
    /// human before it can progress.
    pub idle: Duration,
    /// Stop after this many ticks fail in a row.
    ///
    /// Some failures never clear on their own: a missing gate config, or a
    /// repository that has moved. Looping on those forever would be a worker
    /// that looks alive and achieves nothing, so it gives up and says why.
    pub give_up_after: usize,
}

impl Default for WorkOptions {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(2),
            give_up_after: 5,
        }
    }
}

/// What the loop saw, reported as it happens rather than at the end.
pub enum Progress<'a> {
    Ticked(&'a TickOutcome),
    Failed(&'a eyre::Report),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkSummary {
    pub ticks: usize,
    pub entries_landed: usize,
    pub entries_evicted: usize,
    pub failures: usize,
}

/// How long to wait before ticking again.
///
/// Progress means there is probably more to do straight away; anything else
/// means waiting is the right move. Separated out from the loop so the pacing
/// can be tested without sleeping.
pub fn pause_after(outcome: &TickOutcome, idle: Duration) -> Duration {
    if outcome.is_progress() {
        Duration::ZERO
    } else {
        idle
    }
}

/// Tick until asked to stop.
///
/// `stop` is checked between ticks rather than during one, so a worker always
/// finishes the candidate it is on. It is a closure rather than a signal check
/// so the loop can be tested without installing handlers.
#[allow(clippy::too_many_arguments)]
pub fn work(
    conn: &mut Connection,
    git: &dyn Git,
    gate: &dyn Gate,
    repo_path: &str,
    base_branch: &str,
    log_dir: &Path,
    options: WorkOptions,
    stop: &dyn Fn() -> bool,
    observe: &dyn Fn(Progress<'_>),
) -> Result<WorkSummary> {
    let mut summary = WorkSummary::default();
    let mut consecutive_failures = 0usize;

    while !stop() {
        let ticked = Worker::new(conn, git, gate, repo_path, base_branch, log_dir)
            .with_interrupt(stop)
            .tick();

        match ticked {
            Ok(outcome) => {
                consecutive_failures = 0;
                summary.ticks += 1;
                summary.record(&outcome);
                observe(Progress::Ticked(&outcome));

                let pause = pause_after(&outcome, options.idle);
                if !pause.is_zero() && !stop() {
                    std::thread::sleep(pause);
                }
            }
            Err(failed) => {
                // A signal reaches every child process, so git or the gate can
                // die mid-command and report a non-zero exit with nothing on
                // stderr. That is the interruption arriving, not a fault, and
                // counting it would end a clean shutdown with a phantom
                // failure and a misleading error.
                if stop() {
                    break;
                }

                consecutive_failures += 1;
                summary.failures += 1;

                if consecutive_failures >= options.give_up_after {
                    return Err(failed.wrap_err(format!(
                        "giving up after {consecutive_failures} consecutive failed ticks"
                    )));
                }

                observe(Progress::Failed(&failed));
                if !stop() {
                    std::thread::sleep(options.idle);
                }
            }
        }
    }

    Ok(summary)
}

impl WorkSummary {
    fn record(&mut self, outcome: &TickOutcome) {
        match outcome {
            TickOutcome::Landed { entries, .. } => self.entries_landed += entries,
            TickOutcome::Blamed {
                blame: Blame::Evicted,
                ..
            } => self.entries_evicted += 1,
            _ => {}
        }
    }
}

impl TickOutcome {
    /// Whether the queue moved.
    ///
    /// Drives both how soon to tick again and whether the outcome is worth
    /// reporting twice in a row: a resident worker repeating "nothing queued"
    /// every interval is noise that buries the lines that matter.
    pub fn is_progress(&self) -> bool {
        matches!(
            self,
            Self::Landed { .. } | Self::Split { .. } | Self::Blamed { .. } | Self::Abandoned { .. }
        )
    }
}

pub struct Worker<'a> {
    conn: &'a mut Connection,
    git: &'a dyn Git,
    gate: &'a dyn Gate,
    repo_path: String,
    base_branch: String,
    log_dir: PathBuf,
    lease_ttl: Duration,
    /// Whether a stop has been asked for. Injected rather than read from
    /// process state, so a tick stays a function of its inputs.
    interrupted: &'a dyn Fn() -> bool,
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
            interrupted: &|| false,
        }
    }

    pub fn with_lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl = ttl;
        self
    }

    /// Tell the tick how to find out that a stop was asked for.
    pub fn with_interrupt(mut self, interrupted: &'a dyn Fn() -> bool) -> Self {
        self.interrupted = interrupted;
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
            return self.land(&candidate, &candidate_sha);
        }

        // A gate that died because the worker was signalled says nothing about
        // the branches in the candidate. If a stop is pending we treat the
        // failure as no verdict at all, which errs towards never evicting
        // something that did not actually fail.
        if (self.interrupted)() {
            return self.abandon(candidate.id, Abandoned::Interrupted);
        }

        self.on_gate_failure(&candidate, &config)
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

    pub(super) const REPO: &str = "/repo";
    const BASE: &str = "main";
    pub(super) const CONFIG: &str = "gate = \"run-the-gate\"\n";

    pub(super) fn base_branch() -> &'static str {
        BASE
    }

    fn base() -> Sha {
        FakeGit::commit(1)
    }

    /// A repository whose base branch is checked out nowhere and which has a
    /// gate configured in every commit.
    fn repo() -> FakeGit {
        repo_with_gate(CONFIG)
    }

    pub(super) fn repo_with_gate(config: &str) -> FakeGit {
        FakeGit::new()
            .with_branch("refs/heads/main", &base())
            .with_file_everywhere(config::CONFIG_PATH, config)
            .with_checkout("/integration", None)
    }

    pub(super) fn logs() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    pub(super) fn queue_branch(conn: &Connection, name: &str, at: &Sha) -> EntryId {
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

    /// A signal reaches the gate's shell too, so its non-zero exit is the
    /// interruption rather than a verdict on the branch.
    #[test]
    fn a_gate_killed_by_a_shutdown_is_not_the_branchs_fault() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::failing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let stopping = || true;
        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .with_interrupt(&stopping)
            .tick()
            .unwrap();

        match outcome {
            TickOutcome::Abandoned {
                requeued, reason, ..
            } => {
                assert_eq!(requeued, 1);
                assert_eq!(reason, Abandoned::Interrupted);
            }
            other => panic!("expected the candidate to be discarded, got {other:?}"),
        }
        assert_eq!(entry_state(&conn, a), "queued");
        assert_eq!(
            entry_attempts(&conn, a),
            0,
            "an interruption must not spend a retry"
        );
    }

    #[test]
    fn a_gate_failure_with_no_shutdown_pending_is_still_a_verdict() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo();
        let gate = FakeGate::failing();
        let dir = logs();
        let a = queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let running = || false;
        let outcome = Worker::new(&mut conn, &git, &gate, REPO, BASE, dir.path())
            .with_interrupt(&running)
            .tick()
            .unwrap();

        assert!(
            matches!(outcome, TickOutcome::Blamed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(entry_attempts(&conn, a), 1);
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

#[cfg(test)]
mod loop_tests {
    use std::cell::Cell;

    use super::tests::{CONFIG, REPO, base_branch, logs, queue_branch, repo_with_gate};
    use super::*;
    use crate::gate::fake::FakeGate;
    use crate::git::fake::FakeGit;
    use crate::{db, gate};

    /// No sleeping in tests: pacing is asserted directly through `pause_after`.
    fn immediate() -> WorkOptions {
        WorkOptions {
            idle: Duration::ZERO,
            give_up_after: 5,
        }
    }

    fn ignore(_: Progress<'_>) {}

    #[test]
    fn progress_is_followed_immediately_and_quiet_is_waited_out() {
        let idle = Duration::from_secs(7);

        assert_eq!(
            pause_after(
                &TickOutcome::Landed {
                    candidate: 1,
                    entries: 1,
                    at: FakeGit::commit(1),
                },
                idle
            ),
            Duration::ZERO
        );
        assert_eq!(
            pause_after(
                &TickOutcome::Split {
                    failed: 1,
                    next: 2,
                    retrying: 1,
                },
                idle
            ),
            Duration::ZERO
        );
        assert_eq!(pause_after(&TickOutcome::Idle, idle), idle);
        assert_eq!(
            pause_after(&TickOutcome::AnotherWorkerHolds { holder_pid: 1 }, idle),
            idle,
            "another worker is making the progress; do not spin"
        );
        assert_eq!(
            pause_after(&TickOutcome::BaseCheckedOut { holders: vec![] }, idle),
            idle,
            "this one needs a human, so waiting is all there is"
        );
    }

    #[test]
    fn progress_is_distinguished_from_having_nothing_to_do() {
        assert!(
            TickOutcome::Landed {
                candidate: 1,
                entries: 1,
                at: FakeGit::commit(1),
            }
            .is_progress()
        );
        assert!(
            TickOutcome::Abandoned {
                candidate: 1,
                requeued: 1,
                reason: Abandoned::BaseMoved,
            }
            .is_progress()
        );
        assert!(!TickOutcome::Idle.is_progress());
        assert!(!TickOutcome::AnotherWorkerHolds { holder_pid: 1 }.is_progress());
        assert!(!TickOutcome::BaseCheckedOut { holders: vec![] }.is_progress());
    }

    #[test]
    fn a_stop_asked_for_up_front_does_nothing_at_all() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo_with_gate(CONFIG);
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let summary = work(
            &mut conn,
            &git,
            &gate,
            REPO,
            base_branch(),
            dir.path(),
            immediate(),
            &|| true,
            &ignore,
        )
        .unwrap();

        assert_eq!(summary, WorkSummary::default());
        assert!(gate.calls().is_empty());
    }

    #[test]
    fn the_loop_runs_until_the_queue_is_empty() {
        let mut conn = db::open_in_memory().unwrap();
        let git = repo_with_gate(CONFIG);
        let gate = FakeGate::passing();
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));
        queue_branch(&conn, "feat/b", &FakeGit::commit(3));

        let drained = Cell::new(false);
        let summary = work(
            &mut conn,
            &git,
            &gate,
            REPO,
            base_branch(),
            dir.path(),
            immediate(),
            &|| drained.get(),
            &|progress| {
                if let Progress::Ticked(TickOutcome::Idle) = progress {
                    drained.set(true);
                }
            },
        )
        .unwrap();

        assert_eq!(summary.entries_landed, 2);
        assert_eq!(summary.failures, 0);
        assert_eq!(
            summary.ticks, 2,
            "one tick to land the batch, one to find nothing left"
        );
    }

    /// A gate that keeps blowing up is a worker that looks alive and achieves
    /// nothing, so the loop gives up and says so.
    #[test]
    fn the_loop_gives_up_after_enough_consecutive_failures() {
        struct AlwaysBroken;
        impl Gate for AlwaysBroken {
            fn run(&self, _: &str, _: &Sha, _: &Path) -> Result<gate::Verdict> {
                Err(eyre!("the gate could not be run"))
            }
        }

        let mut conn = db::open_in_memory().unwrap();
        let git = repo_with_gate(CONFIG);
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let failed = work(
            &mut conn,
            &git,
            &AlwaysBroken,
            REPO,
            base_branch(),
            dir.path(),
            WorkOptions {
                idle: Duration::ZERO,
                give_up_after: 3,
            },
            &|| false,
            &ignore,
        )
        .unwrap_err();

        let reported = format!("{failed:?}");
        assert!(
            reported.contains("3 consecutive failed ticks"),
            "the error should say why it stopped: {reported}"
        );
    }

    /// A signal kills the child processes too, so the command a tick was in the
    /// middle of fails. On the way out that is the interruption, not a fault.
    #[test]
    fn a_failure_caused_by_the_shutdown_itself_is_not_counted() {
        struct DiesOnTheSignal<'a> {
            stopping: &'a Cell<bool>,
        }
        impl Gate for DiesOnTheSignal<'_> {
            fn run(&self, _: &str, _: &Sha, _: &Path) -> Result<gate::Verdict> {
                // The signal arrives while the gate is running, killing it.
                self.stopping.set(true);
                Err(eyre!("killed"))
            }
        }

        let mut conn = db::open_in_memory().unwrap();
        let git = repo_with_gate(CONFIG);
        let stopping = Cell::new(false);
        let gate = DiesOnTheSignal {
            stopping: &stopping,
        };
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let summary = work(
            &mut conn,
            &git,
            &gate,
            REPO,
            base_branch(),
            dir.path(),
            immediate(),
            &|| stopping.get(),
            &ignore,
        )
        .unwrap();

        assert_eq!(
            summary.failures, 0,
            "a clean shutdown must not end with a phantom failure"
        );
        assert_eq!(summary.ticks, 0);
    }

    /// The budget counts failures in a row, not failures in total. A transient
    /// problem must not eventually stop a healthy worker.
    #[test]
    fn a_failure_followed_by_success_resets_the_budget() {
        struct BreaksTwice {
            left: Cell<usize>,
        }
        impl Gate for BreaksTwice {
            fn run(&self, _: &str, _: &Sha, _: &Path) -> Result<gate::Verdict> {
                let left = self.left.get();
                if left > 0 {
                    self.left.set(left - 1);
                    return Err(eyre!("transient trouble"));
                }
                Ok(gate::Verdict::Passed)
            }
        }

        let mut conn = db::open_in_memory().unwrap();
        let git = repo_with_gate(CONFIG);
        let gate = BreaksTwice { left: Cell::new(2) };
        let dir = logs();
        queue_branch(&conn, "feat/a", &FakeGit::commit(2));

        let drained = Cell::new(false);
        let summary = work(
            &mut conn,
            &git,
            &gate,
            REPO,
            base_branch(),
            dir.path(),
            WorkOptions {
                idle: Duration::ZERO,
                give_up_after: 3,
            },
            &|| drained.get(),
            &|progress| {
                if let Progress::Ticked(TickOutcome::Idle) = progress {
                    drained.set(true);
                }
            },
        )
        .unwrap();

        assert_eq!(summary.failures, 2);
        assert_eq!(
            summary.entries_landed, 1,
            "two failures in a row then success, under a budget of three"
        );
    }
}
