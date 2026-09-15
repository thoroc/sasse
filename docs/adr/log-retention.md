# Branch ADR: log-retention

## Meta
- **Branch**: `feat/log-retention`
- **Type**: feat
- **Status**: accepted
- **Created**: 2026-09-15
- **Author**: thoroc
- **PR**: None. This repository has no remote; the decision landed directly on `main`.

## Problem Statement
### Context
Every candidate the queue assembles produces a gate log, and nothing has ever
removed one. The queue records a `run` row pointing at each log, and both
accumulate for as long as the queue is used.

Three measurements shaped this decision:

- **The database is not the problem.** Six candidates occupy a single 65KB
  SQLite page. Row growth is a few hundred bytes per candidate, so a decade of
  heavy use is single-digit megabytes. Only the log files are unbounded.
- **sasse's own gate produces 484 bytes.** `cargo test --quiet` is negligible.
  But the gate is whatever someone writes in `sasse.toml`, and a verbose Node or
  Python suite is one to fifty megabytes per run. sasse cannot know in advance
  how chatty a gate is, so the policy has to work without that knowledge.
- **Log volume rises exactly when things go wrong.** One end-to-end run produced
  six candidates for two merges because bisection ran, against roughly one
  candidate per batch when everything is green. Growth therefore correlates with
  failure, and failures are precisely the logs someone wants to read.

That last point is what rules out the obvious answer. "Keep the failures, delete
the rest" removes around ninety per cent of the volume for free and never loses
anything wanted, but it is unbounded in exactly the circumstance retention
exists to survive.

### Goals
- A maximum disk cost that can be stated and relied upon, rather than a
  heuristic that usually holds.
- No single gate run can consume the whole allowance by itself.
- A failed candidate's reason for failing remains recoverable indefinitely, long
  after its bytes are gone.
- Existing configurations keep working without being edited.

### Non-Goals
- Bounding the database. It was measured and it does not need bounding.
- Aging out the small per-run record kept in the database. That is a second
  retention policy and is deliberately not adopted here.
- Compressing logs. It was considered and rejected below.
- Shipping logs anywhere off the machine.

## Decision Record
### Options Considered

**A hard byte budget, spent verdict-first.**
Pros: the only option that yields a real maximum. A log directory bounded at a
declared size is bounded whatever the gate does. Spending it verdict-first, so a
passing candidate's log goes as soon as it lands, means the allowance is used
almost entirely on failures, which are the only logs anyone reads.
Cons: a burst of failures can evict an older failure that was still wanted, and
how far back history reaches cannot be predicted from the setting alone.

**Age-based retention.**
Pros: predictable in time, and matches how people look for a log, which is by
when it happened rather than by how many runs ago it was.
Cons: no size bound whatsoever. A fifty megabyte gate failing repeatedly across
a fourteen day window is fifty gigabytes, which is the precise failure the policy
was meant to prevent.
Rejected because it guarantees the wrong axis.

**Count-based retention.**
Pros: the simplest possible rule, with no size accounting and no clock.
Cons: bounds neither bytes nor time. It is only a real bound if gate output
happens to be uniform in size, and sasse has no way to know that.
Rejected for the same reason as age-based, with less to recommend it.

**Verdict-only retention with no cap.**
Pros: removes most of the volume at no cost and never discards anything a person
would want, since a green gate log has no readers.
Cons: unbounded when every candidate fails, which is when logs multiply.
Rejected as insufficient on its own, though its central insight, that a passing
log has no readers, is adopted as the eviction order inside the budget.

### Chosen Solution
Gate logs are bounded by a hard byte budget, each log is capped by keeping its
head and tail, and a pruned failure leaves its tail behind in the database.

Concretely, three mechanisms:

**A budget over the log directory.** `log_budget` in `sasse.toml`, defaulting to
a modest size, is a ceiling the directory never exceeds. Pruning runs inside the
tick, after a run has been recorded, where the worker already holds the lease. It
spends the allowance in a fixed order: logs belonging to candidates that passed
are removed first, since nobody reads a green gate log, and only then are the
oldest failures removed until the directory is under budget. The log of a
candidate that is still building or being gated is never touched.

**A cap on each log, applied as it is written.** `max_log_size` bounds one file
by writing its first and last chunk and dropping the middle, leaving a marker
saying how much was dropped. The head carries the gate command and its startup
output, where a misconfigured gate announces itself; the tail carries the
failure. The middle of a test run is almost always the cases that passed. Without
this cap a single pathological run can fill the whole budget and evict everything
else, leaving a history exactly one entry deep whose eviction explained nothing.

**A tail kept in the database, for failures.** When a failed candidate's log is
removed, its last lines are stored on the `run` row first. The verdict, command,
exit code, timestamp and the actual reason it failed then survive indefinitely at
a couple of kilobytes each. This is what allows the budget to be aggressive
without the queue ever losing the explanation for a failure. Tails are not kept
for passing runs, which have nothing to explain.

`sasse logs` shows a stored tail when the file is gone, labelled as pruned rather
than presented as a complete log. A `sasse prune` command applies the same policy
out of band, with a dry run that reports what it would remove.

### Rationale
The three mechanisms answer three different failure modes, and none of them
substitutes for another.

The budget is the only one of the four options that produces a number a person
can rely on. Age and count both bound something other than the resource that
actually runs out, and they do so on the basis of an assumption about gate output
that sasse is in no position to make.

The per-log cap exists because a budget alone degenerates. A two hundred megabyte
log against a two hundred megabyte budget is a history one entry deep, and the
eviction that made room for it conveyed nothing. Capping each file at a small
fraction of the budget keeps at least a hundred logs in it.

The stored tail is what makes the aggressive parts acceptable. The objection to
any byte budget is that it eventually deletes a failure someone wanted; the
objection to truncation is that it cuts something that mattered. Keeping the last
lines of every failure permanently answers both in the case that actually
matters, which is wanting to know why something failed rather than wanting the
complete transcript.

Compression was rejected because it is not a bound. It was measured at 3.7 times
on real test output, which moves the slope and not the asymptote, and it would
have meant declaring a hard budget and then implementing something that could
still exceed it.

This is also not in tension with the lesson taken from task-spooler, that results
outlive the queue so they can be reached after the task list is gone.
task-spooler bounded its own retained history with `TS_MAXFINISHED`. Retention is
the other half of that principle rather than a contradiction of it, and the
stored tail is what preserves the part of it that mattered.

## Implementation
### Key Changes
- `src/config.rs`: `log_budget` and `max_log_size`, parsed from human sizes with
  defaults, so an existing `sasse.toml` keeps working unedited.
- `src/gate.rs`: head-and-tail truncation as the log is written, so the cap costs
  one pass and never holds a large log in memory.
- `migrations/0004`: a column on `run` for the retained tail.
- `src/queue/store.rs`: recording a tail, and the queries that decide what a
  prune removes.
- `src/worker.rs`: pruning after a recorded run, inside the tick.
- `src/main.rs`: `sasse prune`, with a dry run, and `sasse logs` falling back to
  a stored tail.

### Testing Strategy
- A log larger than the cap keeps its head and tail and states how much went.
- A log smaller than the cap is left exactly as it was, byte for byte.
- Pruning removes a passing candidate's log before any failure's, and stops as
  soon as the directory is under budget.
- A prune never removes the log of a candidate that is building or being gated.
- A pruned failure still reports why it failed, from the stored tail.
- A passing run stores no tail, since it has nothing to explain.
- The budget holds against a directory deliberately taken over it.

## Challenges & Solutions
The tension was between a bound and an explanation: every mechanism that
guarantees a size can, in principle, delete the thing someone needed. It was
resolved by separating the two resources. Bytes on disk are bounded hard, because
they are what runs out. The explanation is moved into the database, where it is
small enough that bounding it is not yet worth doing, and where it is no longer
in competition with the transcript.

An earlier draft of this decision kept only the verdict on the row and let the
explanation go with the file. Working through the consequence, that a failure
would be recorded as having failed with no line of why, made that plainly worse
than storing a tail for what it costs.

## Impact Assessment
- **Performance**: truncation is a single pass over output already being written.
  A prune is a directory listing and a few unlinks per tick, on a path that has
  just run a test suite.
- **Security**: gate output can contain whatever the gate prints, so the retained
  tail is a copy of a small part of it inside the queue database. It never leaves
  the machine, and the database is already as sensitive as the logs beside it.
  Worth knowing if a gate is ever pointed at something that prints a secret.
- **Maintenance**: two more settings, and a second place a log's content lives.
  `logs` has to make clear which of the two it is showing, or a truncated tail
  will be read as a complete log and the missing middle mistaken for the end.

## Risks & Pitfalls
- **Risk**: a burst of failures evicts an older failure that was still wanted.
  **Mitigation**: the stored tail survives the eviction, so what remains is the
  full transcript of recent failures plus the reason for every older one.
- **Risk**: the retained tails grow without their own bound. At roughly two
  kilobytes per failed run, ten thousand failures is about twenty megabytes of
  database.
  **Mitigation**: accepted deliberately. It is three orders of magnitude below
  the log budget it protects, and adding a second retention policy to save
  twenty megabytes would be the more expensive mistake.
- **Risk**: someone reads a truncated log as complete and takes the dropped
  middle for the end of the run.
  **Mitigation**: the marker states how much was dropped, and `logs` labels a
  stored tail as a tail. This is a presentation problem and it stays one only if
  the labelling is never quietly dropped for being noisy.
- **Risk**: a gate that writes an enormous single line defeats a line-based cap.
  **Mitigation**: the cap is in bytes rather than lines.

## Outcome & Lessons
Pending. To be filled in once the queue has run long enough against a real gate
to know whether the default budget is the right order of magnitude, and whether
anyone has actually wanted a log that the policy had already removed.
