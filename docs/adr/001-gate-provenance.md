# Branch ADR: gate-provenance

## Meta
- **Branch**: `feat/worker-tick`
- **Type**: feat
- **Status**: accepted
- **Created**: 2026-09-14
- **Author**: thoroc
- **PR**: None. This repository has no remote; the decision landed directly on `main`.

## Problem Statement
### Context
The gate is the command sasse runs against a candidate to decide whether it may
land. It is declared in a `sasse.toml` committed to the repository, so that
everyone queueing against that repository gets an identical gate. That is what
makes it one queue rather than several.

A queued branch can modify `sasse.toml` like any other file. The worker
therefore has to decide which commit it reads the gate from, and the answer is
not cosmetic: the worker runs that command on a developer's own machine, as that
developer's user, with no container and no remote CI boundary in between. sasse
has no remote by design, so there is no separate build account to absorb the
consequences of running something unreviewed.

### Goals
- The gate is discoverable from the repository, and identical for everyone
  queueing against a given base branch.
- Queueing a branch is not equivalent to granting that branch code execution on
  the worker's machine.
- A gate change that has landed takes effect without an operator restarting
  anything.

### Non-Goals
- Sandboxing or containerising the gate. The gate is trusted code once landed;
  this decision is only about which commit gets to define it.
- Inspecting or validating what the gate command does.
- Per-entry gates. That was settled separately: a queue whose entries are gated
  differently is not one queue.

## Decision Record
### Options Considered

**Read from the base branch tip, re-read per candidate.**
Pros: the gate always comes from a commit that already passed the queue, so it
has been through whatever review the queue represents. Re-reading per candidate
means a landed gate change takes effect on the next candidate with no restart.
Cons: a branch that legitimately updates the gate is itself gated under the old
gate, so a gate change and the code that depends on it cannot land together.

**Read from the candidate being gated.**
Pros: self-consistent. A branch that changes the gate is tested under its new
gate, so gate and code move together and can land in one merge.
Cons: enqueuing a branch becomes arbitrary code execution on the worker's
machine as the worker's user. `sasse enqueue` would become exactly as privileged
as checking the branch out and running its code directly, which defeats the
purpose of having a gate at all.
Rejected because the threat it creates is larger than the coupling it solves,
and it creates that threat on a developer's workstation rather than on
disposable CI infrastructure.

**Read from the base tip, but refuse to auto-land any candidate that modifies
`sasse.toml`.**
Pros: the same safety as reading from the base, plus gate changes become
deliberate and visible rather than silently deferred by one merge.
Cons: introduces an entry class that requires a human, and therefore another
state to model and surface.
Rejected as more machinery than a single-developer queue warrants. It remains
the natural extension if deferred gate changes turn out to surprise people in
practice.

**Resolve the gate once when the worker starts and pin it for that worker's
lifetime.**
Pros: cheapest, and the gate provably cannot change mid-run.
Cons: a landed gate change is ignored until someone restarts the worker, and
nothing signals that the running gate is stale.
Rejected because a worker silently running a gate that no longer matches the
repository is a confusing failure, and the cost it saves is one file read per
candidate.

### Chosen Solution
The worker reads `sasse.toml` from the base branch tip, re-reading it for each
candidate it builds.

Concretely, at the start of every tick the worker resolves the configured base
ref to a commit, reads the `sasse.toml` blob out of that commit, parses it, and
uses the resulting gate for the candidate it is about to assemble. The gate is
read from the commit itself, not from the integration checkout's working tree,
so nothing a candidate places on disk can influence which command runs. The base
commit the gate came from is recorded alongside the candidate, so the gate in
force for any past merge is recoverable after the fact.

Three behaviours follow from this and are part of the decision rather than
implementation detail:

- The gate is re-resolved per candidate, not cached for the worker's lifetime, so
  a landed gate change takes effect on the next candidate with no restart and no
  operator action.
- A `sasse.toml` that is missing or malformed at the base tip fails the tick
  loudly. There is deliberately no fallback to a built-in default gate, because a
  silent fallback would be a second route to running a command nobody chose.
- A candidate's own `sasse.toml`, whether added, modified or deleted, is read by
  nothing. It becomes the gate only once it has landed and has therefore become
  the base tip.

### Rationale
The base branch tip is the only commit in the system that has already passed the
queue. Reading the gate from it means the command the worker executes has always
been reviewed and landed, which keeps `sasse enqueue` an unprivileged operation.

Re-reading per candidate rather than pinning at startup costs one file read and
removes a whole class of confusion about which gate a long-running worker is
actually using.

The accepted cost is that a gate change lands one merge before it takes effect,
so a branch that changes the gate and depends on the change must be split. That
is an inconvenience with an obvious workaround, whereas the alternative is a
privilege escalation with none.

## Implementation
### Key Changes
- `src/worker/` (pending): the tick resolves the base tip, reads `sasse.toml`
  from that commit, and uses the resulting gate for the candidate it builds.
- The gate is read out of the commit rather than off the working tree, so the
  integration checkout's contents cannot influence which command runs.

### Testing Strategy
- Against the in-memory git fake: a candidate whose branch changes the gate is
  gated with the base's gate, not its own.
- Against a real repository: a landed gate change takes effect on the following
  candidate without restarting the worker.
- A malformed `sasse.toml` at the base tip fails the tick loudly rather than
  falling back to a default gate, since a silent fallback would be a second way
  to run an unintended command.

## Challenges & Solutions
The tension is real rather than invented: reading from the base is safe but
splits a gate change across two merges, and reading from the candidate is
ergonomic but makes enqueueing privileged. It was resolved by asking which
failure is recoverable. A split merge is an inconvenience discovered
immediately; unreviewed code execution on a workstation is discovered after the
fact, if at all.

## Impact Assessment
- **Performance**: one additional object read per candidate. Negligible against
  a gate that runs a test suite.
- **Security**: this is the substance of the decision. It keeps `sasse enqueue`
  from being equivalent to running a branch's code, which matters more here than
  in a hosted queue because the worker runs on a developer's own machine as
  their own user.
- **Maintenance**: gate changes want their own merge. This needs to be stated in
  the README, or it will be rediscovered as a bug report.

## Risks & Pitfalls
- **Risk**: a branch that updates the gate and relies on the update is gated
  under the old gate and fails confusingly.
  **Mitigation**: land gate changes on their own, and have the tick report which
  commit the gate was read from so the mismatch is visible rather than mysterious.
- **Risk**: someone works around the split by pointing the gate at a script
  inside the repository, so `sasse.toml` stops changing while the gate's actual
  behaviour moves with each candidate, quietly restoring the rejected option.
  **Mitigation**: accepted and documented. Closing it would require reading the
  whole gate closure from the base, which is out of proportion to a
  single-developer queue.
- **Risk**: the deferred-by-one-merge behaviour surprises people.
  **Mitigation**: accepted. The third option above is the prepared answer if it
  does.

## Outcome & Lessons
Pending. To be filled in once the worker tick has run against a real repository
for long enough to know whether the one-merge deferral is an irritation in
practice or merely a footnote.
