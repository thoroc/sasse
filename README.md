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
sasse status --repo . --base main --db queue.db
sasse tick --repo . --base main --integration ../integration --db queue.db
sasse work --repo . --base main --integration ../integration --db queue.db
sasse promote 7 --repo . --base main --db queue.db
sasse dequeue 7 --repo . --base main --db queue.db
sasse logs 12 --repo . --base main --db queue.db
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

`status` reads the queue and changes nothing. It shows the base tip, who holds
the lease and for how long, the candidate in flight, what is waiting, and what
recently merged or was evicted and why.

`promote` moves a waiting entry to the front, and `dequeue` takes one out.
Neither will touch an entry that is inside a candidate: it is mid-gate, and
pulling it out from under the worker would leave a candidate referring to
something no longer in the queue. A hand removal is recorded as an eviction
reading `removed by hand`, so a decision stays distinguishable from a verdict.

`logs` with no argument lists recent gate runs with their verdicts; with a
candidate it prints that candidate's runs and the tail of the last one. Gate
logs outlive the queue rows that point at them, so an old one is still readable
after the queue has moved on.

## Configuration

`sasse.toml`, committed to the repository:

```toml
gate = "cargo test --quiet"
max_batch = 8
max_attempts = 3
```

Only `gate` is required. There is deliberately no default gate: a worker that
fell back to a built-in command when the config was missing or malformed would
be a second route to running something nobody chose.

The gate is read from the **base branch tip**, not from the candidate being
gated, so a queued branch cannot choose the command that runs on your machine.
One consequence is worth knowing before it surprises you: a change to
`sasse.toml` takes effect one merge *after* it lands, so a gate change wants its
own merge rather than riding along with the code that depends on it. The
reasoning and the rejected alternatives are in
[docs/adr/gate-provenance.md](docs/adr/gate-provenance.md).

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

- `migrations/0001_init.sql`, the queue schema.
- `src/queue/state.rs`, the entry and candidate state machines.
- `src/queue/outcome.rs`, the per-candidate verdict for an entry, which decides
  whether a failure spends part of that entry's retry budget.
- `src/queue/model.rs`, the bisection decision.
- `src/queue/lease.rs`, the single-worker lease: acquire or attach, renew
  against a fencing token, and reclaim a dead holder's lease along with the
  candidate it abandoned.
- `src/git.rs` and `src/git/`, the git operations behind a trait, with a real
  implementation over the `git` command and an in-memory fake for tests. A merge
  conflict and a base branch that moved are returned as values, because both are
  verdicts the queue acts on rather than failures.
- `src/config.rs`, the committed `sasse.toml`.
- `src/gate.rs`, running the gate against an assembled candidate, behind a trait.
- `src/worker.rs`, the tick: lease, assemble, gate, land or bisect, release,
  plus the loop that repeats it.
- `src/shutdown.rs`, turning a signal into a request to stop between ticks.
- `src/db.rs`, the migration runner.

Not yet written: anything that prunes old gate logs, and any handling of a
repository with more than one base branch beyond keeping their queues separate.

## Development

```sh
mise run build
mise run test
mise run lint
```

## Prior art worth reading

- Zuul (OpenStack) documents the speculative-execution model and its
  invalidation cascade better than either vendor.
- bors-ng for the batch-and-bisect strategy this follows.

## Licence

MIT
