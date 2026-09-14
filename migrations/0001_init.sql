-- An entry is a request to integrate one branch into one base branch.
-- A candidate is an ordered batch of entries tested against one base commit.
-- A run is one execution of the gate against one candidate.
--
-- The CHECK constraints here restate the invariants the queue depends on, so a
-- bug in the worker cannot leave the database in a state the worker will later
-- trust.

CREATE TABLE entry (
    id           INTEGER PRIMARY KEY,
    repo_path    TEXT    NOT NULL,
    base_branch  TEXT    NOT NULL,
    branch       TEXT    NOT NULL,
    branch_sha   TEXT    NOT NULL,
    state        TEXT    NOT NULL CHECK (state IN ('queued', 'batched', 'merged', 'evicted')),
    -- Incremented only when this entry was the culprit of a failed
    -- candidate. See candidate_entry.outcome.
    attempts     INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    priority     INTEGER NOT NULL DEFAULT 0,
    evict_reason TEXT,
    enqueued_at  TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL
);

-- Identity is (repo, base, branch, branch_sha): re-pushing the branch is a new
-- request, not the same one. Only live entries are constrained, so the history
-- of merged and evicted attempts for a given sha is retained.
CREATE UNIQUE INDEX entry_live_identity
    ON entry (repo_path, base_branch, branch, branch_sha)
    WHERE state IN ('queued', 'batched');

CREATE INDEX entry_ready
    ON entry (repo_path, base_branch, priority DESC, id)
    WHERE state = 'queued';

CREATE TABLE candidate (
    id            INTEGER PRIMARY KEY,
    repo_path     TEXT    NOT NULL,
    base_branch   TEXT    NOT NULL,
    -- Base tip when the candidate was built. Re-checked immediately before the
    -- fast-forward; if the base has moved, the result is void.
    base_sha      TEXT    NOT NULL,
    -- The commit the gate actually ran against, and the only commit the base is
    -- ever fast-forwarded to.
    candidate_sha TEXT,
    state         TEXT    NOT NULL CHECK (state IN ('building', 'testing', 'passed', 'failed', 'superseded')),
    -- Set when this candidate was produced by bisecting a failed parent.
    parent_id     INTEGER REFERENCES candidate (id),
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    -- Nothing may be tested or merged without a known candidate commit.
    CHECK (state IN ('building', 'superseded') OR candidate_sha IS NOT NULL)
);

CREATE INDEX candidate_active
    ON candidate (repo_path, base_branch, id)
    WHERE state IN ('building', 'testing');

CREATE TABLE candidate_entry (
    candidate_id INTEGER NOT NULL REFERENCES candidate (id) ON DELETE CASCADE,
    entry_id     INTEGER NOT NULL REFERENCES entry (id) ON DELETE CASCADE,
    -- Merge order within the candidate. Bisection preserves it.
    position     INTEGER NOT NULL CHECK (position >= 0),
    -- What happened to this entry in this candidate. Lives here rather than on
    -- the entry because one entry joins several candidates over its life and
    -- needs a separate verdict in each.
    --
    -- 'culprit' is an entry that failed its own gate, isolated by bisection.
    -- 'skipped' is an entry that was never gated on its own because a
    -- batch-mate was the culprit. Only 'culprit' spends any of the entry's
    -- retry budget; without that distinction one flaky branch drains the
    -- budget of every branch batched alongside it.
    outcome      TEXT    NOT NULL DEFAULT 'pending'
                 CHECK (outcome IN ('pending', 'passed', 'culprit', 'skipped')),
    PRIMARY KEY (candidate_id, entry_id),
    UNIQUE (candidate_id, position)
);

CREATE TABLE run (
    id           INTEGER PRIMARY KEY,
    candidate_id INTEGER NOT NULL REFERENCES candidate (id) ON DELETE CASCADE,
    command      TEXT    NOT NULL,
    exit_code    INTEGER,
    log_path     TEXT,
    started_at   TEXT    NOT NULL,
    finished_at  TEXT
);

CREATE INDEX run_by_candidate ON run (candidate_id, id);
