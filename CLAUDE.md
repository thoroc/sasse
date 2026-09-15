# CLAUDE.md

Guidance for agents working in this repository. `README.md` explains what sasse
is and why; `CONTRIBUTING.md` is the human workflow. This file is the short list
of things that are easy to get wrong here.

## Commands

```sh
mise run hooks:install   # once per clone
mise run ci              # what CI runs: check, then test
mise run check           # every linter, declared once in hk.pkl
mise run fix             # and fix what can be fixed
mise run test            # cargo nextest run
```

Run the linters through `mise run check`, not by invoking `cargo fmt`, `clippy`,
`markdownlint` or `shellcheck` directly. The set lives in `hk.pkl` and the
pre-commit hook, `mise run check` and CI all run that same set; a check reached
another way is a check that can drift.

`mise.toml` puts `./target/debug` on `PATH`, so `sasse` inside this repository is
always the binary just compiled.

## Layout

One crate. `src/lib.rs` is the module list, `src/main.rs` is the clap CLI and the
only place that wires the real implementations together.

- `src/queue/` is the queue: `state.rs` the state machines, `model.rs` the
  bisection decision, `outcome.rs` whether a failure spends an entry's retry
  budget, `lease.rs` the single-worker lease, `store.rs` every read and write of
  the rows.
- `src/worker.rs` is the tick, and the loop around it.
- `src/git/` is the git operations behind a trait: `command.rs` shells out,
  `fake.rs` is the in-memory test double. `src/gate.rs` and `src/notify.rs` are
  behind traits for the same reason.
- `src/logs.rs` decides which logs go, `src/retention.rs` applies that to the
  database and the disk, `src/bytes.rs` parses `200MB`.
- `src/db.rs` is the migration runner, `src/shutdown.rs` turns a signal into a
  request to stop between ticks.

Tests are unit tests in the module they cover, using the fakes. The CI smoke
test drives the built binary against a throwaway repository, because several
real bugs here were only reachable without the fakes.

## Rules that are enforced, not remembered

**`migrations/*.sql` is append-only.** Never edit or delete a landed migration.
The runner records which versions it has applied, not what they said, so an edit
makes the schema depend on when a queue was created. Adding a new migration is
always the answer. `scripts/check-migrations-append-only.sh` refuses the commit,
and CI checks again against the pull request's base.

**An accepted ADR is immutable except for its status.** Supersede rather than
edit. New records go under `docs/adr/` via the `adr` CLI and are scored by `adr
check`; an empty Options Considered section has recorded nothing. `docs/adr/` is
excluded from markdownlint on purpose, because the `adr` template owns that
structure.

**Everything lands on `main` through a pull request**, including for the owner.
Direct pushes are refused and `ci` is the required check. Work on a
`feat/`/`fix/`/`docs/` branch, conventional commits, one concern per commit.

**Prose in markdown is wrapped at 80 columns by hand.** MD013 is set to 100 so a
one-character overshoot does not fail a commit, not as licence to leave a
paragraph unwrapped. Rewrap the paragraph you touched.

## Invariants worth holding before changing the worker

The full list is in `README.md`, and these three are the ones a plausible-looking
change breaks:

1. **Merge exactly what was gated.** The base is only ever fast-forwarded to the
   candidate commit that passed. Never make a fresh merge commit after the gate.
2. **The gate and `on_settle` are read from the base branch tip**, never from the
   candidate under test, so a queued branch cannot choose what runs on the
   machine. See `docs/adr/gate-provenance.md`.
3. **Only the entry isolated as the culprit spends an attempt.** A skipped
   batch-mate, a base that moved mid-gate, a dead worker and an interrupted gate
   all requeue for free.

Invariants 1 and 3 in the README are also `CHECK` constraints and a partial
unique index in the schema. If a change needs the database to accept a new
shape, that is a signal to re-read the invariant, not to drop the constraint.

## Operational footguns

- **The base branch must be checked out nowhere.** The integration checkout runs
  on a detached HEAD. `git update-ref` will move a checked-out branch with no
  warning, leaving that checkout's index disagreeing with its HEAD, so sasse
  refuses to move a base branch any checkout holds.
- **A `sasse.toml` change takes effect one merge after it lands**, because of the
  gate provenance rule above. Give a gate change its own merge.
- **`hk run pre-push` needs its stdin closed**: `hk run pre-push </dev/null`. A
  pre-push hook reads the refs being pushed from stdin and otherwise looks hung.
