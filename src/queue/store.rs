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

/// An entry as a status listing needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub id: EntryId,
    pub branch: String,
    pub branch_sha: Sha,
    pub state: EntryState,
    pub attempts: u32,
    pub priority: i64,
    pub evict_reason: Option<String>,
}

/// The queue as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Waiting, in the order they will be tried.
    pub queued: Vec<StatusEntry>,
    /// Currently inside a candidate.
    pub batched: Vec<StatusEntry>,
    /// Merged or evicted, most recently settled first.
    pub settled: Vec<StatusEntry>,
    pub active: Option<Candidate>,
}

/// Read the queue without changing anything.
pub fn snapshot(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    settled_limit: usize,
) -> Result<Snapshot> {
    Ok(Snapshot {
        queued: status_entries(
            conn,
            repo_path,
            base_branch,
            "state = 'queued'",
            "priority DESC, id ASC",
            None,
        )?,
        batched: status_entries(
            conn,
            repo_path,
            base_branch,
            "state = 'batched'",
            "id ASC",
            None,
        )?,
        settled: status_entries(
            conn,
            repo_path,
            base_branch,
            "state IN ('merged', 'evicted')",
            "updated_at DESC, id DESC",
            Some(settled_limit),
        )?,
        active: active_candidate(conn, repo_path, base_branch)?,
    })
}

fn status_entries(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    predicate: &str,
    order: &str,
    limit: Option<usize>,
) -> Result<Vec<StatusEntry>> {
    // The predicate and ordering are fixed strings chosen here, never values
    // from outside; the parameters that vary are bound.
    let limit = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT id, branch, branch_sha, state, attempts, priority, evict_reason
         FROM entry
         WHERE repo_path = ?1 AND base_branch = ?2 AND {predicate}
         ORDER BY {order}{limit}"
    );

    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map((repo_path, base_branch), |row| {
        Ok((
            row.get::<_, EntryId>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, u32>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(
            |(id, branch, sha, state, attempts, priority, evict_reason)| {
                Ok(StatusEntry {
                    id,
                    branch,
                    branch_sha: Sha::parse(&sha)?,
                    state: EntryState::parse(&state)?,
                    attempts,
                    priority,
                    evict_reason,
                })
            },
        )
        .collect()
}

/// Why an entry left the queue by hand rather than by verdict.
pub const REMOVED_BY_HAND: &str = "removed by hand";

/// The result of changing one entry by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Amendment {
    Applied,
    /// It is not waiting, so there is nothing to change. An entry inside a
    /// candidate is mid-gate, and pulling it out from under the worker would
    /// leave a candidate referring to something no longer in the queue.
    NotWaiting(EntryState),
    Unknown,
}

/// Take a waiting entry out of the queue.
///
/// Recorded as an eviction with a reason saying it was deliberate, rather than
/// as a new state: it is out of the queue and it did not merge, which is what
/// evicted already means. The reason is what distinguishes a decision from a
/// verdict.
pub fn dequeue(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    entry: EntryId,
) -> Result<Amendment> {
    let Some(state) = entry_state(conn, repo_path, base_branch, entry)? else {
        return Ok(Amendment::Unknown);
    };
    if state != EntryState::Queued {
        return Ok(Amendment::NotWaiting(state));
    }

    conn.execute(
        "UPDATE entry SET state = 'evicted', evict_reason = ?2, updated_at = ?3
         WHERE id = ?1",
        (entry, REMOVED_BY_HAND, now_stamp()),
    )?;
    Ok(Amendment::Applied)
}

/// Move a waiting entry to the front.
///
/// Selection orders by priority descending then by id, so one above every other
/// waiting entry is enough. Promoting a second entry puts it ahead of the
/// first, which keeps promotions themselves in the order they were asked for.
pub fn promote(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    entry: EntryId,
) -> Result<Amendment> {
    let Some(state) = entry_state(conn, repo_path, base_branch, entry)? else {
        return Ok(Amendment::Unknown);
    };
    if state != EntryState::Queued {
        return Ok(Amendment::NotWaiting(state));
    }

    conn.execute(
        "UPDATE entry
         SET priority = (
                 SELECT COALESCE(MAX(priority), 0) + 1
                 FROM entry
                 WHERE repo_path = ?2 AND base_branch = ?3 AND state = 'queued'
             ),
             updated_at = ?4
         WHERE id = ?1",
        (entry, repo_path, base_branch, now_stamp()),
    )?;
    Ok(Amendment::Applied)
}

fn entry_state(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    entry: EntryId,
) -> Result<Option<EntryState>> {
    let found: Option<String> = conn
        .query_row(
            "SELECT state FROM entry
             WHERE id = ?1 AND repo_path = ?2 AND base_branch = ?3",
            (entry, repo_path, base_branch),
            |row| row.get(0),
        )
        .optional()?;

    found.map(|state| EntryState::parse(&state)).transpose()
}

/// One execution of the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: i64,
    pub candidate_id: i64,
    pub command: String,
    pub exit_code: Option<i32>,
    /// The log outlives this row, so it may point at a file that is still there
    /// long after the queue has forgotten why it mattered.
    pub log_path: Option<String>,
    pub started_at: String,
}

/// Gate runs for one candidate, oldest first.
pub fn runs(conn: &Connection, candidate_id: i64) -> Result<Vec<Run>> {
    let mut statement = conn.prepare(
        "SELECT id, candidate_id, command, exit_code, log_path, started_at
         FROM run WHERE candidate_id = ?1 ORDER BY id ASC",
    )?;
    let rows = statement.query_map([candidate_id], row_to_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The most recent gate runs against this base branch, newest first.
pub fn recent_runs(
    conn: &Connection,
    repo_path: &str,
    base_branch: &str,
    limit: usize,
) -> Result<Vec<Run>> {
    let mut statement = conn.prepare(
        "SELECT r.id, r.candidate_id, r.command, r.exit_code, r.log_path, r.started_at
         FROM run r
         JOIN candidate c ON c.id = r.candidate_id
         WHERE c.repo_path = ?1 AND c.base_branch = ?2
         ORDER BY r.id DESC
         LIMIT ?3",
    )?;
    let rows = statement.query_map((repo_path, base_branch, limit as i64), row_to_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        candidate_id: row.get(1)?,
        command: row.get(2)?,
        exit_code: row.get(3)?,
        log_path: row.get(4)?,
        started_at: row.get(5)?,
    })
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
    fn a_waiting_entry_can_be_taken_out_by_hand() {
        let conn = db::open_in_memory().unwrap();
        let id = queue(&conn, "feat/unwanted", 1, 0);

        assert_eq!(dequeue(&conn, REPO, BASE, id).unwrap(), Amendment::Applied);

        let snap = snapshot(&conn, REPO, BASE, 10).unwrap();
        assert!(snap.queued.is_empty());
        assert_eq!(snap.settled.len(), 1);
        assert_eq!(snap.settled[0].state, EntryState::Evicted);
        assert_eq!(
            snap.settled[0].evict_reason.as_deref(),
            Some(REMOVED_BY_HAND),
            "a deliberate removal must be distinguishable from a verdict"
        );
    }

    /// Pulling an entry out from under the worker would leave a candidate
    /// referring to something no longer in the queue.
    #[test]
    fn an_entry_inside_a_candidate_cannot_be_taken_out() {
        let mut conn = db::open_in_memory().unwrap();
        let id = queue(&conn, "feat/in-flight", 1, 0);
        let entry = Entry {
            id,
            branch: "feat/in-flight".into(),
            branch_sha: sha(1),
            attempts: 0,
        };
        open_candidate(
            &mut conn,
            REPO,
            BASE,
            &sha(200),
            std::slice::from_ref(&entry),
            None,
        )
        .unwrap();

        assert_eq!(
            dequeue(&conn, REPO, BASE, id).unwrap(),
            Amendment::NotWaiting(EntryState::Batched)
        );
    }

    #[test]
    fn dequeueing_something_that_is_not_there_says_so() {
        let conn = db::open_in_memory().unwrap();
        assert_eq!(dequeue(&conn, REPO, BASE, 404).unwrap(), Amendment::Unknown);
    }

    #[test]
    fn an_entry_from_another_base_branch_is_not_reachable() {
        let conn = db::open_in_memory().unwrap();
        let elsewhere = enqueue(&conn, REPO, "release", "feat/other", &sha(9)).unwrap();
        assert_eq!(
            dequeue(&conn, REPO, BASE, elsewhere).unwrap(),
            Amendment::Unknown
        );
    }

    #[test]
    fn a_promoted_entry_is_selected_first() {
        let conn = db::open_in_memory().unwrap();
        queue(&conn, "feat/first", 1, 0);
        queue(&conn, "feat/second", 2, 0);
        let urgent = queue(&conn, "feat/urgent", 3, 0);

        assert_eq!(
            promote(&conn, REPO, BASE, urgent).unwrap(),
            Amendment::Applied
        );

        let batch = select_batch(&conn, REPO, BASE, 8).unwrap();
        assert_eq!(
            branches(&batch),
            vec!["feat/urgent", "feat/first", "feat/second"]
        );
    }

    /// Promotions stay in the order they were asked for.
    #[test]
    fn a_later_promotion_goes_ahead_of_an_earlier_one() {
        let conn = db::open_in_memory().unwrap();
        let first = queue(&conn, "feat/a", 1, 0);
        let second = queue(&conn, "feat/b", 2, 0);
        queue(&conn, "feat/c", 3, 0);

        promote(&conn, REPO, BASE, first).unwrap();
        promote(&conn, REPO, BASE, second).unwrap();

        let batch = select_batch(&conn, REPO, BASE, 8).unwrap();
        assert_eq!(branches(&batch), vec!["feat/b", "feat/a", "feat/c"]);
    }

    #[test]
    fn a_settled_entry_cannot_be_promoted() {
        let conn = db::open_in_memory().unwrap();
        let id = queue(&conn, "feat/done", 1, 0);
        conn.execute("UPDATE entry SET state = 'merged' WHERE id = ?1", [id])
            .unwrap();

        assert_eq!(
            promote(&conn, REPO, BASE, id).unwrap(),
            Amendment::NotWaiting(EntryState::Merged)
        );
    }

    #[test]
    fn promotion_shows_up_in_the_status_listing() {
        let conn = db::open_in_memory().unwrap();
        let id = queue(&conn, "feat/urgent", 1, 0);
        promote(&conn, REPO, BASE, id).unwrap();

        let snap = snapshot(&conn, REPO, BASE, 10).unwrap();
        assert!(
            snap.queued[0].priority > 0,
            "a promotion the operator cannot see is not much use"
        );
    }

    #[test]
    fn gate_runs_are_readable_for_a_candidate_and_across_the_branch() {
        let mut conn = db::open_in_memory().unwrap();
        let id = queue(&conn, "feat/a", 1, 0);
        let entry = Entry {
            id,
            branch: "feat/a".into(),
            branch_sha: sha(1),
            attempts: 0,
        };
        let candidate = open_candidate(
            &mut conn,
            REPO,
            BASE,
            &sha(200),
            std::slice::from_ref(&entry),
            None,
        )
        .unwrap();

        record_run(&conn, candidate, "gate one", Some(0), "/logs/one.log").unwrap();
        record_run(&conn, candidate, "gate two", Some(1), "/logs/two.log").unwrap();

        let for_candidate = runs(&conn, candidate).unwrap();
        assert_eq!(for_candidate.len(), 2);
        assert_eq!(for_candidate[0].command, "gate one", "oldest first");
        assert_eq!(for_candidate[1].exit_code, Some(1));

        let recent = recent_runs(&conn, REPO, BASE, 10).unwrap();
        assert_eq!(recent[0].command, "gate two", "newest first");
        assert_eq!(recent[0].log_path.as_deref(), Some("/logs/two.log"));
    }

    #[test]
    fn runs_from_another_base_branch_are_not_listed() {
        let conn = db::open_in_memory().unwrap();
        assert!(recent_runs(&conn, REPO, "release", 10).unwrap().is_empty());
    }

    #[test]
    fn a_snapshot_separates_waiting_from_batched_from_settled() {
        let mut conn = db::open_in_memory().unwrap();
        let waiting = queue(&conn, "feat/waiting", 1, 0);
        let inside = queue(&conn, "feat/inside", 2, 0);
        let gone = queue(&conn, "feat/gone", 3, 0);

        let entry = Entry {
            id: inside,
            branch: "feat/inside".into(),
            branch_sha: sha(2),
            attempts: 0,
        };
        let candidate = open_candidate(
            &mut conn,
            REPO,
            BASE,
            &sha(200),
            std::slice::from_ref(&entry),
            None,
        )
        .unwrap();
        conn.execute(
            "UPDATE entry SET state = 'evicted', evict_reason = 'gave up' WHERE id = ?1",
            [gone],
        )
        .unwrap();

        let snap = snapshot(&conn, REPO, BASE, 10).unwrap();
        assert_eq!(
            snap.queued.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![waiting]
        );
        assert_eq!(
            snap.batched.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![inside]
        );
        assert_eq!(
            snap.settled.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![gone]
        );
        assert_eq!(
            snap.settled[0].evict_reason.as_deref(),
            Some("gave up"),
            "why an entry was evicted is part of the status"
        );
        assert_eq!(snap.active.map(|c| c.id), Some(candidate));
    }

    #[test]
    fn a_snapshot_caps_how_much_history_it_returns() {
        let conn = db::open_in_memory().unwrap();
        for n in 1..=5u8 {
            let id = queue(&conn, &format!("feat/{n}"), n, 0);
            conn.execute("UPDATE entry SET state = 'merged' WHERE id = ?1", [id])
                .unwrap();
        }

        assert_eq!(snapshot(&conn, REPO, BASE, 2).unwrap().settled.len(), 2);
    }

    #[test]
    fn an_empty_queue_snapshots_to_nothing() {
        let conn = db::open_in_memory().unwrap();
        let snap = snapshot(&conn, REPO, BASE, 10).unwrap();
        assert!(snap.queued.is_empty());
        assert!(snap.batched.is_empty());
        assert!(snap.settled.is_empty());
        assert_eq!(snap.active, None);
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
