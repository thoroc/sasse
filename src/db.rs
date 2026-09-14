use std::path::Path;
use std::time::Duration;

use eyre::{Result, WrapErr};
use rusqlite::Connection;

const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_worker_lease.sql")),
];

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .wrap_err_with(|| format!("opening queue database at {}", path.display()))?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    // WAL so a reader listing the queue never blocks the worker mid-merge.
    conn.pragma_update(None, "journal_mode", "wal")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    // Workers race to acquire the lease. Without a busy timeout the loser gets
    // SQLITE_BUSY rather than waiting its turn for the write lock, which would
    // turn an ordinary race into an error.
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)
            .wrap_err_with(|| format!("applying migration {version}"))?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_version(conn: &Connection) -> i32 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn migrating_is_idempotent() {
        let conn = open_in_memory().unwrap();
        let first = schema_version(&conn);
        migrate(&conn).unwrap();
        assert_eq!(schema_version(&conn), first);
    }

    #[test]
    fn an_entry_state_outside_the_lifecycle_is_rejected() {
        let conn = open_in_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO entry (repo_path, base_branch, branch, branch_sha, state, enqueued_at, updated_at)
             VALUES ('/repo', 'main', 'feat/x', 'abc', 'nonsense', '', '')",
            [],
        );
        assert!(err.is_err(), "the schema must reject unknown entry states");
    }

    /// Invariant: nothing is tested or merged without a known candidate commit,
    /// so the commit that passed the gate is always the commit merged.
    #[test]
    fn a_candidate_cannot_be_tested_without_a_candidate_sha() {
        let conn = open_in_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO candidate (repo_path, base_branch, base_sha, candidate_sha, state, created_at, updated_at)
             VALUES ('/repo', 'main', 'base', NULL, 'testing', '', '')",
            [],
        );
        assert!(err.is_err(), "a testing candidate must name its commit");
    }

    #[test]
    fn the_same_branch_sha_cannot_be_queued_twice_while_live() {
        let conn = open_in_memory().unwrap();
        let insert = "INSERT INTO entry (repo_path, base_branch, branch, branch_sha, state, enqueued_at, updated_at)
                      VALUES ('/repo', 'main', 'feat/x', 'abc', 'queued', '', '')";
        conn.execute(insert, []).unwrap();
        assert!(conn.execute(insert, []).is_err());
    }

    /// The same identity may appear again once the earlier attempt is history,
    /// so a branch evicted for a flaky gate can be requeued.
    #[test]
    fn a_settled_entry_does_not_block_requeueing_the_same_sha() {
        let conn = open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO entry (repo_path, base_branch, branch, branch_sha, state, enqueued_at, updated_at)
             VALUES ('/repo', 'main', 'feat/x', 'abc', 'evicted', '', '')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entry (repo_path, base_branch, branch, branch_sha, state, enqueued_at, updated_at)
             VALUES ('/repo', 'main', 'feat/x', 'abc', 'queued', '', '')",
            [],
        )
        .unwrap();
    }

    fn seed_candidate(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO candidate (repo_path, base_branch, base_sha, candidate_sha, state, created_at, updated_at)
             VALUES ('/repo', 'main', 'base', 'cand', 'testing', '', '')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn seed_entry(conn: &Connection, branch_sha: &str) -> i64 {
        conn.execute(
            "INSERT INTO entry (repo_path, base_branch, branch, branch_sha, state, enqueued_at, updated_at)
             VALUES ('/repo', 'main', 'feat/x', ?1, 'batched', '', '')",
            [branch_sha],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn an_entry_joins_a_candidate_with_no_verdict_yet() {
        let conn = open_in_memory().unwrap();
        let candidate_id = seed_candidate(&conn);
        let entry_id = seed_entry(&conn, "abc");
        conn.execute(
            "INSERT INTO candidate_entry (candidate_id, entry_id, position) VALUES (?1, ?2, 0)",
            (candidate_id, entry_id),
        )
        .unwrap();

        let outcome: String = conn
            .query_row(
                "SELECT outcome FROM candidate_entry WHERE candidate_id = ?1 AND entry_id = ?2",
                (candidate_id, entry_id),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "pending");
    }

    #[test]
    fn an_outcome_outside_the_verdicts_is_rejected() {
        let conn = open_in_memory().unwrap();
        let candidate_id = seed_candidate(&conn);
        let entry_id = seed_entry(&conn, "abc");
        let result = conn.execute(
            "INSERT INTO candidate_entry (candidate_id, entry_id, position, outcome)
             VALUES (?1, ?2, 0, 'nonsense')",
            (candidate_id, entry_id),
        );
        assert!(result.is_err(), "the schema must reject unknown outcomes");
    }

    /// The schema and the Rust enum have to agree on the set of verdicts. If
    /// they drift, a value legal in one becomes a runtime failure in the other.
    #[test]
    fn every_outcome_the_code_can_produce_is_accepted_by_the_schema() {
        use crate::queue::Outcome;

        let conn = open_in_memory().unwrap();
        let candidate_id = seed_candidate(&conn);
        let outcomes = [
            Outcome::Pending,
            Outcome::Passed,
            Outcome::Culprit,
            Outcome::Skipped,
        ];

        for (position, outcome) in outcomes.iter().enumerate() {
            let entry_id = seed_entry(&conn, &format!("sha{position}"));
            conn.execute(
                "INSERT INTO candidate_entry (candidate_id, entry_id, position, outcome)
                 VALUES (?1, ?2, ?3, ?4)",
                (candidate_id, entry_id, position as i64, outcome.as_str()),
            )
            .unwrap_or_else(|e| panic!("schema rejected outcome {outcome}: {e}"));
        }
    }
}
