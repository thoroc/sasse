-- Relax the rule about when a candidate must name a commit.
--
-- Migration 0001 required a candidate_sha for every state except 'building' and
-- 'superseded'. That wrongly forbids a candidate that never assembled: a batch
-- containing a branch which conflicts with the base produces no commit at all,
-- and is nonetheless genuinely 'failed'.
--
-- The invariant worth enforcing is narrower. A candidate that was gated, or that
-- landed, must name the commit it was gated as, because that is the commit the
-- base is fast-forwarded to. 'building', 'failed' and 'superseded' may all
-- legitimately have no commit.
--
-- SQLite cannot alter a CHECK constraint, so the table is rebuilt. The migration
-- runner turns foreign key enforcement off around each migration and runs
-- pragma_foreign_key_check before committing, which is the procedure SQLite
-- documents for exactly this.

CREATE TABLE candidate_rebuilt (
    id            INTEGER PRIMARY KEY,
    repo_path     TEXT    NOT NULL,
    base_branch   TEXT    NOT NULL,
    base_sha      TEXT    NOT NULL,
    candidate_sha TEXT,
    state         TEXT    NOT NULL CHECK (state IN ('building', 'testing', 'passed', 'failed', 'superseded')),
    parent_id     INTEGER REFERENCES candidate (id),
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    CHECK (state NOT IN ('testing', 'passed') OR candidate_sha IS NOT NULL)
);

INSERT INTO candidate_rebuilt
    (id, repo_path, base_branch, base_sha, candidate_sha, state, parent_id, created_at, updated_at)
SELECT
    id, repo_path, base_branch, base_sha, candidate_sha, state, parent_id, created_at, updated_at
FROM candidate;

DROP TABLE candidate;

ALTER TABLE candidate_rebuilt RENAME TO candidate;

CREATE INDEX candidate_active
    ON candidate (repo_path, base_branch, id)
    WHERE state IN ('building', 'testing');
