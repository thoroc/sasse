-- Exactly one worker may integrate into a given base branch of a given repo at
-- a time. The lease is that exclusion, expressed as a row so it is visible,
-- survives a reboot, and cannot be left behind by a process that died badly.
--
-- Expiry is the authority. A holder renews well before expires_at; a lease past
-- expires_at may be taken by anyone. Checking whether the holder's pid is still
-- alive is only an optimisation that allows reclaiming sooner, and it is
-- deliberately one-directional: a missing pid permits early reclaim, a present
-- pid never extends a lease, because pids are reused and a live pid is not
-- proof the original holder is still running.

CREATE TABLE worker_lease (
    repo_path   TEXT    NOT NULL,
    base_branch TEXT    NOT NULL,
    -- Used only for the liveness optimisation described above.
    holder_pid  INTEGER NOT NULL,
    -- Fencing token, unique per acquisition. Renew and release are conditional
    -- on it, so a worker whose lease was reclaimed underneath it finds out
    -- rather than carrying on and merging.
    token       TEXT    NOT NULL,
    acquired_at TEXT    NOT NULL,
    renewed_at  TEXT    NOT NULL,
    expires_at  TEXT    NOT NULL,
    PRIMARY KEY (repo_path, base_branch)
);
