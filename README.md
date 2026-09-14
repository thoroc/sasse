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
5. Queue state is persisted. A reboot mid-batch must not lose the queue.

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
- `src/db.rs`, the migration runner.

Not yet written: the git operations, the worker loop, and the CLI beyond
`sasse migrate`.

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
