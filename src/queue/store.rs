//! Reading and writing the queue.
//!
//! Every function here is one step the worker takes, so the worker reads as a
//! decision table rather than as SQL. State changes that must not be seen
//! half-applied are wrapped in a transaction here rather than at the call site.

use eyre::{Result, WrapErr, eyre};
use rusqlite::{Connection, OptionalExtension};

use crate::db::now_stamp;
use crate::git::Sha;
use crate::queue::{CandidateState, EntryId, EntryState, Outcome};

/// An entry as the worker needs it: what to merge, and how much of its retry
/// budget is already spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: EntryId,
    pub branch: String,
    pub branch_sha: Sha,
    pub attempts: u32,
}

/// A candidate the worker is partway through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: i64,
    /// The base commit this candidate was assembled against.
    pub base_sha: Sha,
    /// In merge order.
    pub entries: Vec<Entry>,
}

/// Add a branch to the queue.
pub fn enqueue(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    branch: &str,
    branch_sha: &Sha,
) -> Result<EntryId> {
    let at = now_stamp();
    conn.execute(
        "INSERT INTO entry
             (repo_path, base_branch, branch, branch_sha, state, enqueued_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?5)",
        (repo_path, base_branch, branch, branch_sha.as_str(), &at),
    )
    .wrap_err_with(|| format!("queueing {branch}"))?;
    Ok(conn.last_insert_rowid())
}

/// The next entries to try, oldest first within a priority.
///
/// An entry with attempts already spent is never batched alongside one that has
/// none. It has demonstrated that it fails on its own, so putting it back in a
/// batch costs every innocent entry beside it a wasted gate run and a trip
/// through bisection, over and over until its retry budget finally runs out.
/// Such an entry is retried by itself instead, and a batch stops short of one
/// rather than absorbing it.
pub fn select_batch(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    limit: usize,
) -> Result<Vec<Entry>> {
    let queued = queued_entries(conn, repo_path, base_branch, limit)?;

    match queued.first() {
        None => Ok(Vec::new()),
        // A retry goes alone.
        Some(first) if first.attempts > 0 => Ok(vec![first.clone()]),
        // Otherwise batch the run of never-failed entries at the front.
        Some(_) => Ok(queued
            .into_iter()
            .take_while(|entry| entry.attempts == 0)
            .collect()),
    }
}

fn queued_entries(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    limit: usize,
) -> Result<Vec<Entry>> {
    let mut statement = conn.prepare(
        "SELECT id, branch, branch_sha, attempts
         FROM entry
         WHERE repo_path = ?1 AND base_branch = ?2 AND state = 'queued'
         ORDER BY priority DESC, id ASC
         LIMIT ?3",
    )?;
    let rows = statement.query_map((repo_path, base_branch, limit as i64), row_to_entry)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(id, branch, sha, attempts)| {
            Ok(Entry {
                id,
                branch,
                branch_sha: Sha::parse(&sha)?,
                attempts,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    const REPO: &str = "/repo";
    const BASE: &str = "main";

    fn sha(seed: u8) -> Sha {
        Sha::parse(&format!("{seed:02x}").repeat(20)).unwrap()
    }

    fn queue(conn: &Connection, branch: &str, seed: u8, attempts: u32) -> EntryId {
        let id = enqueue(conn, REPO, BASE, branch, &sha(seed)).unwrap();
        if attempts > 0 {
            conn.execute(
                "UPDATE entry SET attempts = ?2 WHERE id = ?1",
                (id, attempts),
            )
            .unwrap();
        }
        id
    }

    fn branches(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.branch.as_str()).collect()
    }

    #[test]
    fn fresh_entries_batch_together_in_queue_order() {
        let conn = db::open_in_memory().unwrap();
        queue(&conn, "feat/a", 1, 0);
        queue(&conn, "feat/b", 2, 0);
        queue(&conn, "feat/c", 3, 0);

        let batch = select_batch(&conn, REPO, BASE, 8).unwrap();
        assert_eq!(branches(&batch), vec!["feat/a", "feat/b", "feat/c"]);
    }

    #[test]
    fn the_batch_respects_the_limit() {
        let conn = db::open_in_memory().unwrap();
        for (n, name) in [(1u8, "feat/a"), (2, "feat/b"), (3, "feat/c")] {
            queue(&conn, name, n, 0);
        }

        let batch = select_batch(&conn, REPO, BASE, 2).unwrap();
        assert_eq!(branches(&batch), vec!["feat/a", "feat/b"]);
    }

    /// It already failed on its own, so batching it again would poison whatever
    /// went with it.
    #[test]
    fn an_entry_being_retried_is_tried_by_itself() {
        let conn = db::open_in_memory().unwrap();
        queue(&conn, "feat/known-bad", 1, 1);
        queue(&conn, "feat/innocent", 2, 0);

        let batch = select_batch(&conn, REPO, BASE, 8).unwrap();
        assert_eq!(branches(&batch), vec!["feat/known-bad"]);
    }

    #[test]
    fn a_batch_stops_short_of_an_entry_being_retried() {
        let conn = db::open_in_memory().unwrap();
        queue(&conn, "feat/a", 1, 0);
        queue(&conn, "feat/b", 2, 0);
        queue(&conn, "feat/known-bad", 3, 2);
        queue(&conn, "feat/d", 4, 0);

        let batch = select_batch(&conn, REPO, BASE, 8).unwrap();
        assert_eq!(
            branches(&batch),
            vec!["feat/a", "feat/b"],
            "the retry and everything behind it wait their turn"
        );
    }

    #[test]
    fn an_empty_queue_yields_an_empty_batch() {
        let conn = db::open_in_memory().unwrap();
        assert!(select_batch(&conn, REPO, BASE, 8).unwrap().is_empty());
    }

    #[test]
    fn entries_for_another_base_branch_are_not_selected() {
        let conn = db::open_in_memory().unwrap();
        queue(&conn, "feat/a", 1, 0);
        enqueue(&conn, REPO, "release", "feat/elsewhere", &sha(9)).unwrap();

        let batch = select_batch(&conn, REPO, BASE, 8).unwrap();
        assert_eq!(branches(&batch), vec!["feat/a"]);
    }
}

/// The candidate the worker has not finished with, if there is one.
///
/// At most one candidate per base branch is ever unfinished, which is what the
/// worker lease guarantees.
pub fn active_candidate(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
) -> Result<Option<Candidate>> {
    let found: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, base_sha FROM candidate
             WHERE repo_path = ?1 AND base_branch = ?2 AND state IN ('building', 'testing')
             ORDER BY id ASC
             LIMIT 1",
            (repo_path, base_branch),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let Some((id, base_sha)) = found else {
        return Ok(None);
    };

    Ok(Some(Candidate {
        id,
        base_sha: Sha::parse(&base_sha)?,
        entries: candidate_entries(conn, id)?,
    }))
}

/// The entries of a candidate, in merge order.
pub fn candidate_entries(conn: &Connection, candidate_id: i64) -> Result<Vec<Entry>> {
    let mut statement = conn.prepare(
        "SELECT e.id, e.branch, e.branch_sha, e.attempts
         FROM candidate_entry ce
         JOIN entry e ON e.id = ce.entry_id
         WHERE ce.candidate_id = ?1
         ORDER BY ce.position ASC",
    )?;
    let rows = statement.query_map([candidate_id], row_to_entry)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(id, branch, sha, attempts)| {
            Ok(Entry {
                id,
                branch,
                branch_sha: Sha::parse(&sha)?,
                attempts,
            })
        })
        .collect()
}

/// Open a candidate over these entries, moving them out of the queue.
///
/// `parent` records the candidate this one was split from, so a bisection's
/// lineage is recoverable.
pub fn open_candidate(
    conn: &mut Connection,
    repo_path: &str,
    base_branch: &str,
    base_sha: &Sha,
    entries: &[Entry],
    parent: Option<i64>,
) -> Result<i64> {
    if entries.is_empty() {
        return Err(eyre!("refusing to open a candidate over no entries"));
    }

    let at = now_stamp();
    let tx = conn.transaction()?;

    tx.execute(
        "INSERT INTO candidate
             (repo_path, base_branch, base_sha, candidate_sha, state, parent_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, NULL, 'building', ?4, ?5, ?5)",
        (repo_path, base_branch, base_sha.as_str(), parent, &at),
    )?;
    let candidate_id = tx.last_insert_rowid();

    for (position, entry) in entries.iter().enumerate() {
        tx.execute(
            "INSERT INTO candidate_entry (candidate_id, entry_id, position, outcome)
             VALUES (?1, ?2, ?3, 'pending')",
            (candidate_id, entry.id, position as i64),
        )?;
        move_entry(&tx, entry.id, EntryState::Batched, &at)?;
    }

    tx.commit()?;
    Ok(candidate_id)
}

/// Record the commit that was assembled, and that it is now being gated.
pub fn mark_testing(conn: &Connection, candidate_id: i64, candidate_sha: &Sha) -> Result<()> {
    set_candidate(
        conn,
        candidate_id,
        CandidateState::Testing,
        Some(candidate_sha),
    )
}

/// Record one execution of the gate. The log outlives this row.
pub fn record_run(
    conn: &Connection,
    candidate_id: i64,
    command: &str,
    exit_code: Option<i32>,
    log_path: &str,
) -> Result<()> {
    let at = now_stamp();
    conn.execute(
        "INSERT INTO run (candidate_id, command, exit_code, log_path, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        (candidate_id, command, exit_code, log_path, &at),
    )?;
    Ok(())
}

/// The candidate passed and its commit is now the base. Everything in it landed.
pub fn land(conn: &mut Connection, candidate_id: i64) -> Result<usize> {
    let at = now_stamp();
    let tx = conn.transaction()?;

    let landed = tx.execute(
        "UPDATE candidate_entry SET outcome = 'passed' WHERE candidate_id = ?1",
        [candidate_id],
    )?;
    tx.execute(
        "UPDATE entry SET state = 'merged', updated_at = ?2
         WHERE id IN (SELECT entry_id FROM candidate_entry WHERE candidate_id = ?1)",
        (candidate_id, &at),
    )?;
    tx.execute(
        "UPDATE candidate SET state = 'passed', updated_at = ?2 WHERE id = ?1",
        (candidate_id, &at),
    )?;

    tx.commit()?;
    Ok(landed)
}

/// Abandon a candidate without blaming anything in it, returning its entries to
/// the queue.
///
/// Their outcomes stay pending, because no verdict was reached, and so no part
/// of anyone's retry budget is spent.
pub fn abandon(conn: &mut Connection, candidate_id: i64, state: CandidateState) -> Result<usize> {
    if !matches!(state, CandidateState::Superseded | CandidateState::Failed) {
        return Err(eyre!("{state} is not a way to abandon a candidate"));
    }

    let at = now_stamp();
    let tx = conn.transaction()?;

    let requeued = tx.execute(
        "UPDATE entry SET state = 'queued', updated_at = ?2
         WHERE state = 'batched'
           AND id IN (SELECT entry_id FROM candidate_entry WHERE candidate_id = ?1)",
        (candidate_id, &at),
    )?;
    tx.execute(
        "UPDATE candidate SET state = ?2, updated_at = ?3 WHERE id = ?1",
        (candidate_id, state.as_str(), &at),
    )?;

    tx.commit()?;
    Ok(requeued)
}

/// What happened to the entry a failed candidate was pinned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blame {
    /// Its retry budget is spent; it is out of the queue.
    Evicted,
    /// It has attempts left and goes back in line.
    Requeued,
}

/// Pin a failed candidate on one entry.
///
/// The culprit spends an attempt and is evicted once its budget is gone.
/// Everything else in the candidate is marked skipped and requeued, spending
/// nothing: they were never gated on their own, so a flaky neighbour must not
/// cost them anything.
pub fn blame(
    conn: &mut Connection,
    candidate_id: i64,
    culprit: EntryId,
    max_attempts: u32,
    reason: &str,
) -> Result<Blame> {
    let at = now_stamp();
    let tx = conn.transaction()?;

    let attempts: u32 =
        tx.query_row("SELECT attempts FROM entry WHERE id = ?1", [culprit], |r| {
            r.get(0)
        })?;
    let spent = attempts + 1;
    let verdict = if spent >= max_attempts {
        Blame::Evicted
    } else {
        Blame::Requeued
    };

    tx.execute(
        "UPDATE candidate_entry SET outcome = ?3
         WHERE candidate_id = ?1 AND entry_id = ?2",
        (candidate_id, culprit, Outcome::Culprit.as_str()),
    )?;
    tx.execute(
        "UPDATE candidate_entry SET outcome = ?2
         WHERE candidate_id = ?1 AND entry_id != ?3",
        (candidate_id, Outcome::Skipped.as_str(), culprit),
    )?;

    match verdict {
        Blame::Evicted => {
            tx.execute(
                "UPDATE entry SET state = 'evicted', attempts = ?2, evict_reason = ?3,
                        updated_at = ?4
                 WHERE id = ?1",
                (culprit, spent, reason, &at),
            )?;
        }
        Blame::Requeued => {
            tx.execute(
                "UPDATE entry SET state = 'queued', attempts = ?2, updated_at = ?3
                 WHERE id = ?1",
                (culprit, spent, &at),
            )?;
        }
    }

    // The skipped entries go back in line untouched.
    tx.execute(
        "UPDATE entry SET state = 'queued', updated_at = ?3
         WHERE state = 'batched'
           AND id != ?2
           AND id IN (SELECT entry_id FROM candidate_entry WHERE candidate_id = ?1)",
        (candidate_id, culprit, &at),
    )?;
    tx.execute(
        "UPDATE candidate SET state = 'failed', updated_at = ?2 WHERE id = ?1",
        (candidate_id, &at),
    )?;

    tx.commit()?;
    Ok(verdict)
}

fn set_candidate(
    conn: &Connection,
    candidate_id: i64,
    state: CandidateState,
    candidate_sha: Option<&Sha>,
) -> Result<()> {
    let at = now_stamp();
    let changed = match candidate_sha {
        Some(sha) => conn.execute(
            "UPDATE candidate SET state = ?2, candidate_sha = ?3, updated_at = ?4 WHERE id = ?1",
            (candidate_id, state.as_str(), sha.as_str(), &at),
        )?,
        None => conn.execute(
            "UPDATE candidate SET state = ?2, updated_at = ?3 WHERE id = ?1",
            (candidate_id, state.as_str(), &at),
        )?,
    };
    if changed == 0 {
        return Err(eyre!("no candidate {candidate_id} to move to {state}"));
    }
    Ok(())
}

fn move_entry(
    conn: &Connection,
    entry: EntryId,
    state: EntryState,
    at: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE entry SET state = ?2, updated_at = ?3 WHERE id = ?1",
        (entry, state.as_str(), at),
    )
}

type EntryRow = (EntryId, String, String, u32);

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}
