# Branch ADR: on-settle-hook

## Meta
- **Branch**: `feat/on-settle-hook`
- **Type**: feat
- **Status**: accepted
- **Created**: 2026-09-15
- **Author**: thoroc
- **PR**: None. The decision landed on `main`; see the repository history.

## Problem Statement
### Context
`sasse work` is meant to be left running. Nothing it does reaches the person who
queued a branch. An entry is blamed, requeued, blamed again, and evicted once its
retry budget is spent, and the only way to discover any of that is to run `sasse
status` and read it.

The failure that matters is silent eviction. A branch queued and then evicted
looks, from the outside, exactly like a branch still waiting: neither has landed.
The queue behaved correctly and recorded everything, and the person who cared
never found out.

task-spooler had the same problem and solved it with `TS_ONFINISH`, a command run
when a job completes. That is already recorded in
`docs/salvage-from-task-spooler.md`, which notes its author's own bug list entry
asking for the hook's output to be logged, because a hook failing quietly was
confusing enough to file against himself.

### Goals
- The person who queued a branch learns when it lands and when it is evicted,
  without polling.
- A broken or hanging hook cannot stop branches landing.
- A broken hook is discoverable, rather than looking the same as a quiet queue.
- Configurations without a hook keep working untouched.

### Non-Goals
- Any particular transport. Desktop notification, `say`, a Slack post or a log
  line are all the hook script's business, not the queue's.
- Reporting internal churn. A requeue or a skip is work that has not finished
  happening.
- Notifying about removals the operator performed by hand.

## Decision Record
### Options Considered

The decision has two axes, and each had real alternatives.

**What triggers it.**

*Per entry, on merge or eviction.* Chosen. One invocation per entry reaching a
terminal state. "feat/x landed" is the thing that was being waited for and
"feat/y evicted after 3 attempts" is the thing that needs acting on. A batch of
four landing produces four notifications, which is proportionate: four branches
somebody cared about.

*Only evictions.* Quietest, on the argument that a branch appearing on the base
is its own confirmation. Rejected because a worker that has quietly stopped
working then looks identical to a queue with nothing to do, and the whole point
of this is removing that ambiguity.

*Per tick outcome.* Fewer invocations when batches are large, and it exposes
bisection progress. Rejected because the notification would then be about the
queue's internals rather than about a branch, and filtering out idle ticks would
become every hook script's first problem.

*Every entry state change.* Most complete, and it would let the hook drive a
dashboard. Rejected because a bisection cascade requeues the same entries
repeatedly, so a four-entry batch with one bad branch emits a stream of events
about work that had not finished happening.

**What happens when the hook fails.**

*Reported but never fatal, with a timeout.* Chosen. A non-zero exit is recorded
and shown in the tick's output, then ignored.

*A failing hook fails the tick.* Consistent with how a missing gate config is
treated, which fails loudly rather than guessing. Rejected because the entry has
already merged by the time the hook runs, so the tick would report failure for
work that succeeded. That is the same misleading pairing that moved pruning to
the start of a tick rather than the end.

*Counted, giving up after several in a row.* Mirrors `give_up_after`. Rejected as
a second failure counter with different semantics from the one already there,
which is a distinction to explain forever in exchange for very little.

*Fire and forget, detached.* Simplest and safest for the queue: no timeout
needed, no hang possible. Rejected, and it is the option most likely to be
proposed again, so the reason is worth stating plainly: a silently failing hook
looks exactly like a queue with nothing to report, which is precisely the failure
this feature exists to remove. Choosing it would make the feature indisting-
uishable from not having built it.

### Chosen Solution
An optional `on_settle` command in `sasse.toml`, run once per entry that reaches
`merged` or `evicted`.

The command receives its context through the environment, mirroring
`SASSE_CANDIDATE`, which the gate already gets: the branch, the outcome as
`merged` or `evicted`, the entry and candidate ids, how many attempts were spent,
the repository and base branch, and the eviction reason, which is empty on a
merge. It runs through a shell, because a hook written by hand in a config file
will contain pipes and quoting.

Four behaviours follow, and they are the decision rather than implementation
detail:

- **A hook failure never fails the tick.** The merge has already happened and is
  not undone by a notifier exiting non-zero. The failure is reported in the
  tick's output instead, which keeps it from being a silent fallback.
- **A hook is bounded by a timeout.** This matters more than the exit code. A
  hook that curls a URL with no timeout of its own, or that prompts, would
  otherwise block the tick indefinitely and wedge the queue far more thoroughly
  than any missed notification. The limit is a fixed ten seconds rather than
  another setting, on the basis that a notifier needing longer is doing something
  that should not be inline.
- **The hook's own output is captured and shown only when it fails.** A working
  notifier should not clutter the tick's output; a broken one should explain
  itself.
- **It fires after the entry's state is committed.** An interruption between the
  two therefore loses the notification rather than the merge, which is the right
  way round.

`on_settle` is optional and absent means no hook and no behavioural change, so
every existing configuration keeps working unedited. `sasse dequeue` does not
fire it: nobody needs telling about a removal they performed themselves.

### Rationale
The feature exists for one failure, silent eviction, and both axes were decided
by asking which option still removes it.

That is what rules out notifying only on eviction: without a positive signal, a
worker that has stopped and a queue with nothing to do are the same observation.
It is also what rules out fire-and-forget, which removes the failure only while
the hook happens to work and restores it, invisibly, the moment the hook breaks.

Per-entry rather than per-tick follows from who the notification is for. The
person waiting has a branch, not a candidate, and a tick is an implementation
detail of how their branch gets tested. Making every hook script filter the
queue's internals would be exporting a problem the queue is better placed to
solve.

Never-fatal follows from an ordering that is already settled elsewhere in this
design. The queue's job is merging; notification is downstream of it. Pruning was
moved to the start of a tick for exactly this reason, so that a broken log
directory could not report a failure immediately after a successful merge. A
broken notifier deserves the same treatment, and for the same reason.

The timeout is the part that is easy to leave out and would hurt most. A non-zero
exit is a small, recoverable annoyance. A hook that hangs stops the queue
entirely, and it would do so while looking like a worker that is busy.

## Implementation
### Key Changes
- `src/config.rs`: an optional `on_settle`.
- `src/notify.rs`: running the command with a bounded timeout, behind a trait so
  the worker's behaviour can be tested without spawning anything.
- `src/queue/store.rs`: `land` and `blame` report which entries settled and how,
  since the worker needs the branch names and the reason to pass on.
- `src/worker.rs`: firing the hook after the state change, and carrying a hook
  failure into the tick's outcome without failing it.

### Testing Strategy
- A landed entry fires the hook once per entry, with the branch and `merged`.
- An evicted entry fires it with `evicted`, the attempt count and the reason.
- A requeued or skipped entry fires nothing.
- A hook exiting non-zero leaves the merge intact and the tick successful, and
  the failure is reported rather than swallowed.
- A hook that sleeps past the timeout is abandoned, and the tick still completes.
- No `on_settle` configured spawns nothing at all.
- `sasse dequeue` fires nothing.
- Against a real repository: a hook that appends to a file records exactly the
  entries that settled, in the order they settled.

## Challenges & Solutions
The tension was between making a broken hook loud and making it harmless, which
pull in opposite directions. Failing the tick is loud and harmful; fire-and-forget
is harmless and silent. Separating the two concerns resolved it: the queue's
behaviour is unaffected, and the reporting channel is the tick's output, which a
person reading `sasse work` is already watching.

The timeout was not part of the original framing and is the more important half of
the failure handling. It surfaced from asking what a hook could do that is worse
than exiting non-zero, and the answer was to not exit at all.

## Impact Assessment
- **Performance**: one short-lived process per settled entry, on a path that has
  just run a test suite. Negligible, and bounded by the timeout.
- **Security**: the hook is a shell command from `sasse.toml`, read from the base
  branch tip like the gate, so the same provenance argument in
  `gate-provenance.md` applies unchanged: a queued branch cannot introduce a hook
  that the worker will run. It receives branch names and eviction reasons in its
  environment, so a hook that forwards them off the machine is forwarding
  repository metadata. That is the hook author's decision and worth knowing.
- **Maintenance**: a second configured command alongside the gate, with the same
  provenance rules and a different failure policy. The difference is deliberate
  and is the substance of this record, so it needs to stay explained.

## Risks & Pitfalls
- **Risk**: a hook silently does nothing useful, for instance a notifier that
  exits zero while failing to notify.
  **Mitigation**: out of reach. The queue can report an exit code and a timeout;
  it cannot verify that a notification arrived.
- **Risk**: ten seconds is wrong for somebody's hook.
  **Mitigation**: accepted for now. A setting is one line to add once there is a
  case for it, and shipping the knob first invites tuning a number nobody has
  measured.
- **Risk**: a hook forwarding branch names to an external service, from a
  repository whose branch names are sensitive.
  **Mitigation**: noted in the impact assessment and in the README rather than
  prevented. The hook exists to send things elsewhere.
- **Risk**: the notification is treated as a guarantee, and a lost one is read as
  the branch not having landed.
  **Mitigation**: firing after the commit makes the queue's record authoritative
  and the notification advisory. `status` remains the source of truth.

## Outcome & Lessons
Pending. To be filled in once a real hook has been configured for long enough to
know whether per-entry notifications on a batching queue are the right volume,
which is the number most likely to be wrong.
