# sasse

A local merge queue. Batch, gate, fast-forward.

## Why the name

`un sas` is the chamber of a canal lock, and an older word for a fine sieve.
Both are what this does: pass changes through one at a time in order, and sift
out the ones that fail. `sasse` is to `sasser` what `mise` is to `mettre`, which
is the naming pattern being followed.

## What problem it solves

Testing a branch in isolation tells you nothing about the state after it lands.
Branch A renames a function, branch B adds a caller of the old name; both are
green, neither conflicts textually, and the base branch breaks when both merge.

So the rule is: gate the merge result, never the branch. Stated as an invariant,
it is Graydon Hoare's Not Rocket Science Rule, to automatically maintain a
repository of code that always passes all the tests.

## Design: batch and bisect

GitLab merge trains and GitHub merge queue optimise for parallel CI capacity.
They build one speculative candidate per queue prefix (`base+A`, `base+A+B`,
`base+A+B+C`) and test all of them at once, trading N pipelines of compute for
one pipeline of wall clock.

That trade is worthless on a single machine, where three parallel gate runs do
not finish sooner, they just contend. So sasse batches instead:

- Test `base+A+B+C` as one candidate. One gate run covers N merges.
- If it passes, fast-forward the base to the candidate commit. All N land.
- If it fails, bisect. Split the batch, retest each half, recurse until a batch
  of one names the culprit. That entry is evicted; the rest are requeued.

Common case is one gate run for the whole batch. Failure costs `log2(N)`
additional rounds, and only when something is actually broken.

## Usage

```sh
sasse migrate --db queue.db
sasse enqueue feat/my-branch --repo . --base main --db queue.db
sasse status --repo . --db queue.db              # every branch with a queue
sasse status --repo . --base main --db queue.db  # one queue, in full
sasse tick --repo . --base main --integration ../integration --db queue.db
sasse work --repo . --base main --integration ../integration --db queue.db
sasse promote 7 --repo . --base main --db queue.db
sasse dequeue 7 --repo . --base main --db queue.db
sasse logs 12 --repo . --base main --db queue.db
sasse prune --repo . --base main --db queue.db --dry-run
```

A tick takes the lease, advances the queue by at most one candidate, and gives
the lease back. `work` is a loop around `tick`: it follows progress immediately,
waits `--interval` when there is nothing to do, and stops on an interrupt after
finishing the candidate it is on. Ticks that fail in a row are counted, and it
gives up after `--give-up-after` of them rather than looping forever on
something that will never clear, such as a missing gate config.

Either command can be interrupted safely. A signal reaches the gate's shell as
well as the worker, so the gate exits non-zero, but that is recorded as an
interruption rather than as a verdict: the candidate is discarded, its entries
are requeued, and nobody's retry budget is charged.

`status` reads the queue and changes nothing. With `--base` it shows that
queue's base tip, who holds the lease and for how long, the candidate in flight,
what is waiting, and what recently merged or was evicted and why. Without
`--base` it lists every base branch in the repository that has a queue, one line
each, discovered from the database rather than from configuration.

A branch stays listed once its queue drains, and one whose ref has since been
deleted is shown with an unresolved tip rather than failing the command: a branch
that is gone but still has queue history is exactly what you would be running
this to find.

Queues on different base branches are fully independent. The worker lease is
held per repository and base branch, so a worker on `main` does not exclude one
on `release`, which is asserted by a test rather than assumed. Serving two
branches is therefore two `sasse work` processes, deliberately: a single worker
covering several branches would reimplement inside one process an exclusion the
lease already provides between them. The reasoning is in
[docs/adr/0003-base-branch-overview.md](docs/adr/0003-base-branch-overview.md).

`promote` moves a waiting entry to the front, and `dequeue` takes one out.
Neither will touch an entry that is inside a candidate: it is mid-gate, and
pulling it out from under the worker would leave a candidate referring to
something no longer in the queue. A hand removal is recorded as an eviction
reading `removed by hand`, so a decision stays distinguishable from a verdict.

`logs` with no argument lists recent gate runs with their verdicts; with a
candidate it prints that candidate's runs and the tail of the last one.

## Being told when something settles

`sasse work` is meant to be left running, and without a hook nothing it does
reaches the person who queued a branch. The failure that matters is silent
eviction: a branch queued and then evicted looks, from outside, exactly like one
still waiting, because neither has landed.

`on_settle` is a command run once for each entry that merges or is evicted. It
is optional, and unset means nothing runs. The context arrives in the
environment:

| variable | meaning |
| --- | --- |
| `SASSE_BRANCH` | the branch that settled |
| `SASSE_OUTCOME` | `merged` or `evicted` |
| `SASSE_REASON` | why it was evicted; empty on a merge |
| `SASSE_ATTEMPTS` | how many attempts it spent |
| `SASSE_ENTRY` | the entry number |
| `SASSE_REPO`, `SASSE_BASE` | which queue |

A requeue or a skip announces nothing, being work that has not finished
happening. `sasse dequeue` announces nothing either: you do not need telling
about a removal you performed yourself.

A hook that fails is reported alongside the outcome and never instead of it. The
merge or the eviction already happened and is not undone by a broken notifier,
so the tick still succeeds and the failure appears as an extra line:

```text
candidate 7 passed; 1 entr(ies) landed at 96a9617
  on_settle for feat-c (merged): exited 127: definitely-not-a-command: not found
```

A hook is also bounded by a ten second timeout, which matters more than the exit
code: one that curls a URL with no timeout of its own, or that prompts, would
otherwise hold the tick open indefinitely and wedge the queue far more
thoroughly than any missed notification. It is read from the base branch tip like
the gate, so a queued branch cannot introduce a command the worker will run. The
reasoning and the rejected alternatives are in
[docs/adr/0004-on-settle-hook.md](docs/adr/0004-on-settle-hook.md).

## Log retention

Gate logs are the only thing in the queue that grows without bound, and how fast
depends entirely on how chatty the configured gate is. Two settings bound it, and
`sasse prune` applies the same policy by hand, with `--dry-run` to see what would
go. They are written under `--logs`, `sasse-logs` by default, and outlive the
queue rows that point at them.

- **`max_log_size` caps one log as it is written.** Past the cap the log keeps its
  head and its tail and loses the middle, with a marker saying how many bytes
  went. The head has the gate command's startup output, the tail has the failure,
  and the middle of a test run is almost always the cases that passed. Without
  this, one pathological run fills the whole budget by itself.
- **`log_budget` caps the directory.** A passing candidate's log is removed as
  soon as it settles, since nobody reads a green gate log, and then the oldest
  failures go until the directory is under budget. Pruning runs at the start of
  each tick and never touches a candidate still being assembled or gated.

A failure's last lines are copied onto its run row before its file is removed, so
the verdict, the command and the actual reason survive indefinitely at about a
kilobyte each. `sasse logs` shows that tail when the file has gone, labelled as a
tail rather than presented as a whole log. That is what makes the budget a real
ceiling rather than a preference: honouring it costs bytes, not explanations.

Measured on a gate printing roughly 330KB per run, an 8KB cap with a 32KB budget
held at 24750 bytes across three files, where the same eight candidates would
otherwise have left 2.6MB. The reasoning and the rejected alternatives are in
[docs/adr/0002-log-retention.md](docs/adr/0002-log-retention.md).

## Configuration

`sasse.toml`, committed to the repository:

```toml
gate = "cargo test --quiet"
max_batch = 8
max_attempts = 3
log_budget = "200MB"
max_log_size = "2MB"
on_settle = "terminal-notifier -message \"$SASSE_BRANCH $SASSE_OUTCOME\""
```

Only `gate` is required. Sizes may be written as `200MB`, `512KB` or a plain
number of bytes; suffixes are powers of 1024. There is deliberately no default
gate: a worker that fell back to a built-in command when the config was missing
or malformed would
be a second route to running something nobody chose.

The gate is read from the **base branch tip**, not from the candidate being
gated, so a queued branch cannot choose the command that runs on your machine.
One consequence is worth knowing before it surprises you: a change to
`sasse.toml` takes effect one merge *after* it lands, so a gate change wants its
own merge rather than riding along with the code that depends on it. The
reasoning and the rejected alternatives are in
[docs/adr/0001-gate-provenance.md](docs/adr/0001-gate-provenance.md).

## Repository layout

The queue requires that **the base branch is checked out nowhere**. The
integration checkout runs on a detached HEAD, and developers work on feature
branches.

That is not a style preference. Verified against git 2.55: `git update-ref` will
move a branch that is checked out, in the current worktree or another one, with
no warning and no refusal, leaving that checkout's index disagreeing with its
HEAD so the entire difference appears as staged changes. Git does not protect
this, so sasse refuses to move a base branch that any checkout holds.

## Invariants

1. Merge exactly what was gated. The base is only ever fast-forwarded to the
   candidate commit that passed. A fresh merge commit made after the gate was
   never tested, which is how hand-rolled queues break.
2. The base must not move between gate and merge. The base sha is recorded when
   the candidate is built and re-checked before the fast-forward.
3. Entry identity is `(repo, base branch, branch, branch sha)`. If the branch is
   repushed, it is a new request and prior results are void.
4. Eviction needs a retry budget. Without one a single flaky test destroys
   throughput; with an unlimited one a genuinely broken branch blocks the queue.
   Only the entry isolated as the culprit spends an attempt. An entry skipped
   because a batch-mate failed requeues for free, so one flaky branch cannot
   drain the budget of everything batched alongside it.
   An entry being retried is also never batched with an entry that has never
   failed. It has already demonstrated that it fails on its own, so batching it
   again would cost every innocent entry beside it a wasted gate run and a trip
   through bisection, repeatedly, until its budget finally ran out.
   Nor is an entry charged for anything that was not its own doing: a base that
   moved mid-gate, a worker that died, or an interrupted gate all requeue for
   free.
5. Queue state is persisted. A reboot mid-batch must not lose the queue.
6. Exactly one worker integrates into a base branch at a time, held as a lease
   with an expiry. Expiry is the authority, so a worker that wedges or dies
   cannot hold a branch indefinitely, and a reclaim clears whatever it left
   mid-gate rather than leaving the queue stuck.

Invariants 1 and 3 are enforced by `CHECK` constraints and a partial unique
index in the schema, not only by the worker, so a bug in the worker cannot leave
a state the worker will later trust.

## Status

Early. What exists:

- `migrations/`, the queue schema, added to and never edited: the initial
  tables, the worker lease, a relaxed rule about when a candidate must name a
  commit, and the retained tail of a pruned log.
- `src/queue/state.rs`, the entry and candidate state machines.
- `src/queue/outcome.rs`, the per-candidate verdict for an entry, which decides
  whether a failure spends part of that entry's retry budget.
- `src/queue/model.rs`, the bisection decision.
- `src/queue/lease.rs`, the single-worker lease: acquire or attach, renew
  against a fencing token, and reclaim a dead holder's lease along with the
  candidate it abandoned.
- `src/queue/store.rs`, every read and write of the queue rows, so the worker
  reads as a decision table rather than as SQL.
- `src/git/`, the git operations behind a trait, with a real
  implementation over the `git` command and an in-memory fake for tests. A merge
  conflict and a base branch that moved are returned as values, because both are
  verdicts the queue acts on rather than failures.
- `src/config.rs`, the committed `sasse.toml`.
- `src/bytes.rs`, sizes written the way people write them.
- `src/logs.rs`, reading a log's tail and deciding which logs go.
- `src/retention.rs`, applying that decision to the database and the disk.
- `src/gate.rs`, running the gate against an assembled candidate, behind a trait.
- `src/notify.rs`, running the on_settle hook with a bounded timeout.
- `src/worker.rs`, the tick: lease, assemble, gate, land or bisect, release,
  plus the loop that repeats it.
- `src/shutdown.rs`, turning a signal into a request to stop between ticks.
- `src/db.rs`, the migration runner.

Not yet written: any bound on the retained log tails themselves, about 20MB per
ten thousand failures, deliberately left alone as three orders of magnitude
below the log budget they protect.

## Development

```sh
mise run hooks:install   # once per clone
mise run ci              # what CI runs: check, then test
mise run check           # every linter
mise run fix             # and fix what can be fixed
mise run test
```

Everything lands on `main` through a pull request, and `ci` is the only required
check. The branch loop, the merge settings, and the commit, ADR and migration
conventions are in [CONTRIBUTING.md](CONTRIBUTING.md).

Linters are declared once, in `hk.pkl`, and every route runs that same set: the
pre-commit hook, `mise run check`, and CI. A check that only CI knows how to run
is a check people discover by having it fail.

`hk install` wires two hooks. Pre-commit fixes and checks the files being
committed. Pre-push adds the test suite, which is deliberately not in
pre-commit: a full suite on every commit is a hook people turn off. Neither hook
is a substitute for CI: a local pass is not a CI pass.

To run the pre-push hook by hand, close its stdin: `hk run pre-push
</dev/null`. A pre-push hook receives the refs being pushed on stdin, so
without that it waits for input forever and looks like a hang.

Alongside the usual formatters and linters there is one project rule enforced
rather than remembered: `scripts/check-migrations-append-only.sh` refuses a
commit that modifies or deletes a migration that has already landed. A database
that applied the old text will never apply the new one, because the runner
records which versions it has run rather than what they said, so editing a
landed migration makes the schema depend on when a queue was created. Adding a
new migration is always the answer.

Tool versions are pinned in `mise.toml` and locked in `mise.lock`, and CI
installs them through mise rather than using whatever the runner image ships, so
a linter cannot pass locally and fail in CI over a version difference.

`.github/workflows/ci.yml` runs on every pull request and on every push to
`main`. It runs `mise run check` and `mise run test`, checks the migrations are
append-only against the pull request's base, checks that no tool version changed
without `mise.lock` being committed, and drives the built binary against a
throwaway repository, because the unit tests use fakes for git and for the gate
and several real bugs here were only reachable without them. One `ci` job gathers
the rest, so requiring that single check requires all of them. Every action is
pinned to a full commit SHA, and `zizmor.yml` lints the workflows themselves.

## Prior art worth reading

- Zuul (OpenStack) documents the speculative-execution model and its
  invalidation cascade better than either vendor.
- bors-ng for the batch-and-bisect strategy this follows.

## Licence

MIT. See [LICENSE](LICENSE).
