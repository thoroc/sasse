//! Exclusion between workers.
//!
//! Exactly one worker may integrate into a given base branch of a given repo.
//! The lease is that exclusion. It is a database row rather than a lock file so
//! it is visible to anyone who can read the queue, survives a reboot with an
//! explicit expiry, and carries none of the ownership and stale-path hazards of
//! a socket or pidfile in a shared directory.
//!
//! Expiry is the authority. A holder renews well before `expires_at`, and a
//! lease past `expires_at` may be taken by anyone. Checking whether the
//! holder's pid still exists is only an optimisation that allows reclaiming
//! sooner, and it is deliberately one-directional: a missing pid permits early
//! reclaim, while a present pid never extends a lease, because pids are reused
//! and a live pid is not proof the original holder is still running.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use eyre::{Result, WrapErr, eyre};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

/// Whether a lease's recorded holder still exists.
///
/// There are only two answers worth acting on. Anything that is not a
/// confirmed absence is treated as presence, so uncertainty never shortens
/// somebody else's lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Gone,
    Present,
}

/// A lease this process holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub repo_path: String,
    pub base_branch: String,
    pub holder_pid: i32,
    /// Fencing token. Renew and release are conditional on it, so a worker
    /// whose lease was reclaimed underneath it finds out rather than carrying
    /// on and merging.
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// What a reclaim had to clean up after the previous holder.
///
/// A worker killed mid-gate leaves a candidate that will never reach a verdict
/// and entries that will never leave the batch. Reclaiming the lease without
/// clearing those would wedge the queue, which is the failure mode
/// task-spooler's own bug list records as recoverable only by a full reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    pub previous_holder_pid: i32,
    pub candidates_superseded: usize,
    pub entries_requeued: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquisition {
    Acquired {
        lease: Lease,
        /// `None` when the lease was simply free.
        reclaimed: Option<Reclaimed>,
    },
    Held {
        holder_pid: i32,
        expires_at: DateTime<Utc>,
    },
}

/// Take the lease, or report who holds it.
///
/// Losing is not an error. A caller that loses attaches to the existing worker
/// rather than failing, which is the shape task-spooler uses to elect a single
/// server: race to acquire, and fall through on loss.
pub fn acquire(
    conn: &mut Connection,
    repo_path: &str,
    base_branch: &str,
    ttl: Duration,
) -> Result<Acquisition> {
    let now = Utc::now();
    let expires_at = now + chrono::Duration::from_std(ttl)?;

    // IMMEDIATE takes the write lock up front, so two workers reading the same
    // absent lease cannot both go on to insert one.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let existing: Option<(i32, String)> = tx
        .query_row(
            "SELECT holder_pid, expires_at FROM worker_lease
             WHERE repo_path = ?1 AND base_branch = ?2",
            (repo_path, base_branch),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let mut reclaimed = None;

    if let Some((holder_pid, held_until)) = existing {
        let held_until = parse_stamp(&held_until)?;

        if held_until > now && liveness(holder_pid) == Liveness::Present {
            // Dropping the transaction rolls back; nothing was written.
            return Ok(Acquisition::Held {
                holder_pid,
                expires_at: held_until,
            });
        }

        let (candidates_superseded, entries_requeued) =
            supersede_orphans(&tx, repo_path, base_branch, now)?;
        reclaimed = Some(Reclaimed {
            previous_holder_pid: holder_pid,
            candidates_superseded,
            entries_requeued,
        });

        tx.execute(
            "DELETE FROM worker_lease WHERE repo_path = ?1 AND base_branch = ?2",
            (repo_path, base_branch),
        )?;
    }

    let holder_pid = std::process::id() as i32;
    let token = mint_token(holder_pid, now);
    tx.execute(
        "INSERT INTO worker_lease
             (repo_path, base_branch, holder_pid, token, acquired_at, renewed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        (
            repo_path,
            base_branch,
            holder_pid,
            &token,
            stamp(now),
            stamp(expires_at),
        ),
    )
    .wrap_err("inserting the worker lease")?;
    tx.commit()?;

    Ok(Acquisition::Acquired {
        lease: Lease {
            repo_path: repo_path.to_string(),
            base_branch: base_branch.to_string(),
            holder_pid,
            token,
            expires_at,
        },
        reclaimed,
    })
}

/// Push the expiry out. Fails if the lease is no longer ours.
///
/// A worker must treat that failure as fatal to its current work: another
/// worker now owns the base branch, so anything this one is midway through
/// merging must not be completed.
pub fn renew(conn: &Connection, lease: &Lease, ttl: Duration) -> Result<Lease> {
    let now = Utc::now();
    let expires_at = now + chrono::Duration::from_std(ttl)?;

    let changed = conn.execute(
        "UPDATE worker_lease SET renewed_at = ?1, expires_at = ?2
         WHERE repo_path = ?3 AND base_branch = ?4 AND token = ?5",
        (
            stamp(now),
            stamp(expires_at),
            &lease.repo_path,
            &lease.base_branch,
            &lease.token,
        ),
    )?;

    if changed == 0 {
        return Err(eyre!(
            "the lease on {} ({}) is no longer ours; it was reclaimed while we held it",
            lease.repo_path,
            lease.base_branch
        ));
    }

    Ok(Lease {
        expires_at,
        ..lease.clone()
    })
}

/// Give up the lease. Returns false if it was not ours to give up.
pub fn release(conn: &Connection, lease: &Lease) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM worker_lease
         WHERE repo_path = ?1 AND base_branch = ?2 AND token = ?3",
        (&lease.repo_path, &lease.base_branch, &lease.token),
    )?;
    Ok(changed == 1)
}

/// Clear what a dead worker left mid-gate.
///
/// Entries are requeued before the candidates are superseded, because they are
/// found by way of those candidates still being active.
fn supersede_orphans(
    tx: &Transaction<'_>,
    repo_path: &str,
    base_branch: &str,
    now: DateTime<Utc>,
) -> Result<(usize, usize)> {
    let at = stamp(now);

    let entries_requeued = tx.execute(
        "UPDATE entry SET state = 'queued', updated_at = ?3
         WHERE state = 'batched'
           AND id IN (
               SELECT ce.entry_id
               FROM candidate_entry ce
               JOIN candidate c ON c.id = ce.candidate_id
               WHERE c.repo_path = ?1
                 AND c.base_branch = ?2
                 AND c.state IN ('building', 'testing')
           )",
        (repo_path, base_branch, &at),
    )?;

    let candidates_superseded = tx.execute(
        "UPDATE candidate SET state = 'superseded', updated_at = ?3
         WHERE repo_path = ?1 AND base_branch = ?2 AND state IN ('building', 'testing')",
        (repo_path, base_branch, &at),
    )?;

    Ok((candidates_superseded, entries_requeued))
}

/// Unique per acquisition.
///
/// It does not need to be unguessable, only distinct: the pid separates
/// concurrent processes, and the timestamp separates one process's successive
/// acquisitions.
fn mint_token(pid: i32, now: DateTime<Utc>) -> String {
    format!("{pid}-{}", now.timestamp_nanos_opt().unwrap_or_default())
}

/// Fixed-width UTC, so the text column orders the same way the instants do.
fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn parse_stamp(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .wrap_err_with(|| format!("parsing the lease timestamp {s:?}"))?
        .with_timezone(&Utc))
}

#[cfg(unix)]
fn liveness(pid: i32) -> Liveness {
    // Signal 0 runs the existence and permission checks without delivering
    // anything.
    //
    // SAFETY: kill is always safe to call; an invalid pid is reported through
    // errno rather than being undefined.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Liveness::Present;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(e) if e == libc::ESRCH => Liveness::Gone,
        // EPERM means a process is there and is not ours. Anything else we
        // cannot interpret. Both leave the lease to expire on its own.
        _ => Liveness::Present,
    }
}

#[cfg(not(unix))]
fn liveness(_pid: i32) -> Liveness {
    Liveness::Present
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;

    use super::*;
    use crate::db;
    use crate::queue::Outcome;

    const TTL: Duration = Duration::from_secs(60);

    fn queue() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        db::open(&path).unwrap();
        (dir, path)
    }

    fn acquired(a: Acquisition) -> (Lease, Option<Reclaimed>) {
        match a {
            Acquisition::Acquired { lease, reclaimed } => (lease, reclaimed),
            Acquisition::Held { holder_pid, .. } => {
                panic!("expected to acquire, but pid {holder_pid} holds it")
            }
        }
    }

    /// A pid that certainly no longer exists: a child we started and reaped.
    fn reaped_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        pid
    }

    fn write_lease(conn: &Connection, holder_pid: i32, expires_at: DateTime<Utc>) {
        conn.execute(
            "INSERT INTO worker_lease
                 (repo_path, base_branch, holder_pid, token, acquired_at, renewed_at, expires_at)
             VALUES ('/repo', 'main', ?1, 'someone-elses-token', ?2, ?2, ?3)",
            (holder_pid, stamp(Utc::now()), stamp(expires_at)),
        )
        .unwrap();
    }

    fn seed_batched_entry(conn: &Connection, candidate_state: &str) -> (i64, i64) {
        conn.execute(
            "INSERT INTO entry (repo_path, base_branch, branch, branch_sha, state, enqueued_at, updated_at)
             VALUES ('/repo', 'main', 'feat/x', 'abc', 'batched', '', '')",
            [],
        )
        .unwrap();
        let entry_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO candidate (repo_path, base_branch, base_sha, candidate_sha, state, created_at, updated_at)
             VALUES ('/repo', 'main', 'base', 'cand', ?1, '', '')",
            [candidate_state],
        )
        .unwrap();
        let candidate_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO candidate_entry (candidate_id, entry_id, position) VALUES (?1, ?2, 0)",
            (candidate_id, entry_id),
        )
        .unwrap();
        (candidate_id, entry_id)
    }

    fn state_of(conn: &Connection, table: &str, id: i64) -> String {
        conn.query_row(
            &format!("SELECT state FROM {table} WHERE id = ?1"),
            [id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn an_unheld_lease_is_acquired() {
        let (_dir, path) = queue();
        let mut conn = db::open(&path).unwrap();
        let (lease, reclaimed) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());
        assert_eq!(lease.holder_pid, std::process::id() as i32);
        assert_eq!(reclaimed, None, "nothing to reclaim from a free lease");
    }

    #[test]
    fn a_live_holder_keeps_its_lease() {
        let (_dir, path) = queue();
        let mut conn = db::open(&path).unwrap();
        acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());

        match acquire(&mut conn, "/repo", "main", TTL).unwrap() {
            Acquisition::Held { holder_pid, .. } => {
                assert_eq!(holder_pid, std::process::id() as i32)
            }
            Acquisition::Acquired { .. } => panic!("a live, unexpired lease must not be taken"),
        }
    }

    #[test]
    fn leases_on_different_base_branches_do_not_exclude_each_other() {
        let (_dir, path) = queue();
        let mut conn = db::open(&path).unwrap();
        acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());
        acquired(acquire(&mut conn, "/repo", "release", TTL).unwrap());
    }

    #[test]
    fn an_expired_lease_is_reclaimed_even_though_its_holder_is_alive() {
        let (_dir, path) = queue();
        let conn = db::open(&path).unwrap();
        // Our own pid, so liveness reports Present. Expiry must still win,
        // otherwise a wedged-but-running worker holds the branch forever.
        write_lease(
            &conn,
            std::process::id() as i32,
            Utc::now() - chrono::Duration::seconds(1),
        );
        drop(conn);

        let mut conn = db::open(&path).unwrap();
        let (_, reclaimed) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());
        assert!(reclaimed.is_some(), "an expired lease is a reclaim");
    }

    #[test]
    fn a_lease_whose_holder_is_gone_is_reclaimed_before_it_expires() {
        let (_dir, path) = queue();
        let conn = db::open(&path).unwrap();
        write_lease(&conn, reaped_pid(), Utc::now() + chrono::Duration::hours(1));
        drop(conn);

        let mut conn = db::open(&path).unwrap();
        let (_, reclaimed) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());
        assert!(
            reclaimed.is_some(),
            "a dead holder should not hold the branch until expiry"
        );
    }

    #[test]
    fn reclaiming_supersedes_an_orphaned_candidate_and_requeues_its_entries() {
        let (_dir, path) = queue();
        let conn = db::open(&path).unwrap();
        let (candidate_id, entry_id) = seed_batched_entry(&conn, "testing");
        write_lease(&conn, reaped_pid(), Utc::now() + chrono::Duration::hours(1));
        drop(conn);

        let mut conn = db::open(&path).unwrap();
        let (_, reclaimed) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());
        let reclaimed = reclaimed.expect("a dead holder is a reclaim");
        assert_eq!(reclaimed.candidates_superseded, 1);
        assert_eq!(reclaimed.entries_requeued, 1);

        assert_eq!(state_of(&conn, "candidate", candidate_id), "superseded");
        assert_eq!(state_of(&conn, "entry", entry_id), "queued");
    }

    /// A superseded candidate reached no verdict, so its entries keep a pending
    /// outcome and spend none of their retry budget.
    #[test]
    fn a_superseded_candidate_costs_its_entries_nothing() {
        let (_dir, path) = queue();
        let conn = db::open(&path).unwrap();
        let (candidate_id, entry_id) = seed_batched_entry(&conn, "testing");
        write_lease(&conn, reaped_pid(), Utc::now() + chrono::Duration::hours(1));
        drop(conn);

        let mut conn = db::open(&path).unwrap();
        acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());

        let outcome: String = conn
            .query_row(
                "SELECT outcome FROM candidate_entry
                 WHERE candidate_id = ?1 AND entry_id = ?2",
                (candidate_id, entry_id),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcome, Outcome::Pending.as_str());

        let attempts: i64 = conn
            .query_row(
                "SELECT attempts FROM entry WHERE id = ?1",
                [entry_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 0);
    }

    #[test]
    fn a_finished_candidate_is_not_disturbed_by_a_reclaim() {
        let (_dir, path) = queue();
        let conn = db::open(&path).unwrap();
        let (candidate_id, _) = seed_batched_entry(&conn, "passed");
        write_lease(&conn, reaped_pid(), Utc::now() + chrono::Duration::hours(1));
        drop(conn);

        let mut conn = db::open(&path).unwrap();
        let (_, reclaimed) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());
        assert_eq!(reclaimed.unwrap().candidates_superseded, 0);
        assert_eq!(state_of(&conn, "candidate", candidate_id), "passed");
    }

    #[test]
    fn renewing_pushes_the_expiry_out() {
        let (_dir, path) = queue();
        let mut conn = db::open(&path).unwrap();
        let (lease, _) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());

        let renewed = renew(&conn, &lease, Duration::from_secs(600)).unwrap();
        assert!(renewed.expires_at > lease.expires_at);
        assert_eq!(renewed.token, lease.token, "renewal keeps the same token");
    }

    /// The fencing check. A worker that lost its lease has to discover that
    /// before it merges anything.
    #[test]
    fn renewing_a_reclaimed_lease_fails() {
        let (_dir, path) = queue();
        let mut conn = db::open(&path).unwrap();
        let (lease, _) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());

        conn.execute("DELETE FROM worker_lease", []).unwrap();
        write_lease(&conn, reaped_pid(), Utc::now() + chrono::Duration::hours(1));

        assert!(
            renew(&conn, &lease, TTL).is_err(),
            "a lease taken from under us must not renew"
        );
    }

    #[test]
    fn releasing_our_own_lease_frees_the_branch() {
        let (_dir, path) = queue();
        let mut conn = db::open(&path).unwrap();
        let (lease, _) = acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());

        assert!(release(&conn, &lease).unwrap());
        acquired(acquire(&mut conn, "/repo", "main", TTL).unwrap());
    }

    #[test]
    fn releasing_a_lease_that_is_not_ours_changes_nothing() {
        let (_dir, path) = queue();
        let conn = db::open(&path).unwrap();
        write_lease(&conn, reaped_pid(), Utc::now() + chrono::Duration::hours(1));

        let not_ours = Lease {
            repo_path: "/repo".into(),
            base_branch: "main".into(),
            holder_pid: 1,
            token: "a-token-we-invented".into(),
            expires_at: Utc::now(),
        };
        assert!(!release(&conn, &not_ours).unwrap());

        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM worker_lease", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "someone else's lease must survive");
    }

    /// Port of task-spooler's `teststartrace.sh`, which races five clients to
    /// become the single server, fifty times over. Same property here: however
    /// many workers start at once, exactly one gets the branch.
    #[test]
    fn concurrent_acquisitions_elect_exactly_one_worker() {
        const ROUNDS: usize = 25;
        const RACERS: usize = 5;

        let (_dir, path) = queue();
        let keeper = db::open(&path).unwrap();

        for round in 0..ROUNDS {
            let winners = Arc::new(AtomicUsize::new(0));

            std::thread::scope(|scope| {
                for _ in 0..RACERS {
                    let path: &Path = &path;
                    let winners = Arc::clone(&winners);
                    scope.spawn(move || {
                        let mut conn = db::open(path).unwrap();
                        if let Acquisition::Acquired { .. } =
                            acquire(&mut conn, "/repo", "main", TTL).unwrap()
                        {
                            winners.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                }
            });

            assert_eq!(
                winners.load(Ordering::SeqCst),
                1,
                "round {round}: exactly one worker must win"
            );

            keeper.execute("DELETE FROM worker_lease", []).unwrap();
        }
    }
}
