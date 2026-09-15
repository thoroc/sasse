# Branch ADR: base-branch-overview

## Meta
- **Branch**: `feat/base-branch-overview`
- **Type**: feat
- **Status**: accepted
- **Created**: 2026-09-15
- **Author**: thoroc
- **PR**: None. This repository has no remote; the decision landed directly on `main`.

## Problem Statement
### Context
Every queue is keyed on `(repo_path, base_branch)`. Entries, candidates and the
worker lease all carry both, so a repository with a `main` queue and a `release`
queue already has two genuinely independent queues, and a test asserts that a
lease on one does not exclude a worker on the other.

What was missing was narrower than "multiple base branches are unsupported". The
isolation works. Two things did not:

- Every command required `--base`, so answering "what queues exist in this
  repository" meant already knowing the answer, or querying SQLite by hand.
- Nothing drove more than one branch, so serving `main` and `release` meant two
  invocations, with no way to see both at once.

A repository with a release branch alongside `main` is ordinary rather than
exotic, so leaving the first of those unaddressed made the tool awkward for a
normal layout.

### Goals
- Answer "what queues exist here" without prior knowledge of the branch names.
- Add no concurrency machinery for something that already works correctly.
- Keep a single queue's detail as readable as it is today.

### Non-Goals
- Aggregating across repositories. The database can hold several, but one
  `sasse.db` per repository is the default and the intended shape.
- Bounding the retained log tails. Deliberately deferred; the reasoning and the
  trigger are recorded in `log-retention.md` under Risks and Pitfalls.
- Any change to how a queue is driven.

## Decision Record
### Options Considered

**Read-only aggregation, one worker per branch.**
Pros: closes the half that was actual friction, at the cost of a query and some
formatting. The branch list comes from the database, so nothing new has to be
declared or kept in sync. Driving is untouched, and the per-`(repo, base)` lease
already makes concurrent workers on different branches safe, which is tested
rather than assumed.
Cons: serving two branches remains two processes to start and two logs to read.

**One worker serving several branches.**
Pros: one process, one log. Operationally tidier for someone running a resident
worker over a repository with a release branch.
Cons: needs a fairness rule so a busy branch cannot starve a quiet one, and puts
scheduling state into a loop whose current appeal is that it holds none: a tick
is a function of persisted state, which is what makes it testable and what lets
a crashed worker be resumed by the next tick rather than recovered.
Rejected because it duplicates, inside one process, an exclusion the lease
already provides between processes, and pays real complexity for it. This is the
option most likely to be proposed again, which is why it is written down here.

**Declare the served branches in `sasse.toml`.**
Pros: the set of served branches becomes reviewable in git, consistent with the
gate being configured there.
Cons: configuration is read from a base branch tip, so asking which base to read
the list of bases from is mildly circular, and adding a branch becomes a commit
rather than an argument. It also duplicates knowledge the database already has.
Rejected as declaring something that can be observed.

**Nothing in the tool, with a documented supervisor pattern.**
Pros: zero new surface area, and one worker per branch under launchd is already
correct.
Cons: leaves no way to answer what queues exist without reading SQLite by hand,
which was the friction that made this a gap.
Rejected because it documents around the problem instead of addressing it.

### Chosen Solution
`--base` becomes optional on `status`. Omitted, it lists every base branch in
that repository that has a queue; given, it shows that queue in full as before.
Driving is unchanged: `tick` and `work` still take exactly one base branch.

Four things follow, and they are part of the decision rather than implementation
detail:

- There is no `--all` flag. A flag that contradicts another flag needs a rule
  about combining them, and the absence of `--base` already expresses "all of
  them" without one.
- The branch list is discovered from the database, over entries, candidates and
  leases together, so a branch whose queue has drained still appears instead of
  vanishing the moment it goes quiet.
- The overview is a summary per branch, not the full listing repeated: base tip,
  who holds the lease, whether a candidate is in flight, and counts of queued,
  batched and settled. Detail stays behind `--base`, because repeating a full
  entry listing for every branch makes the common case unreadable.
- A branch whose ref no longer resolves is shown with an unresolved tip rather
  than failing the listing. A deleted branch that still has queue history is
  exactly the thing someone would be running this command to find.

Every other command still requires `--base`, because each acts on one queue and
inferring which would be a guess about intent.

### Rationale
The gap was in observation, not in isolation, and the two halves deserved
different answers.

Isolation was already correct and tested, so the work was to stop requiring
knowledge the tool already had. Discovering the branch list from the database
rather than from configuration follows from that: the database is where the
answer lives, and anything declared alongside it is a second copy to keep
current.

Leaving driving alone is the substantive half of the decision. A resident worker
over several branches is the option that looks like progress and is not: the
lease already excludes two workers on the same branch and already permits two on
different ones, so a multi-branch loop would reimplement inside one process what
is already guaranteed between processes, and would trade away the property that
makes a tick testable. One process per branch is less tidy and more correct.

Dropping the `--all` flag in favour of an optional `--base` is a smaller point
but the same instinct: the narrower interface needs no rule about which
combinations are legal.

## Implementation
### Key Changes
- `src/queue/store.rs`: a query for the distinct base branches a repository has
  queues for, and a per-branch summary.
- `src/main.rs`: `--base` becomes optional on `status`, with the overview when it
  is absent and the existing detail when it is present.

### Testing Strategy
- A repository with queues on two branches lists both, and a repository with one
  lists one.
- A branch whose entries have all settled still appears, rather than vanishing
  when its queue drains.
- A branch known only to a lease or only to a candidate still appears.
- Queues belonging to another repository in the same database are not listed.
- `--base` still produces exactly the detail it produced before.
- A branch whose ref no longer resolves is listed rather than failing the
  command.

## Challenges & Solutions
The temptation was to read "handles multiple base branches" as "one worker
handles them", which is the larger and more satisfying change. Separating the
request into isolation, observation and driving showed that isolation was already
done, observation was the real gap, and driving was a solved problem wearing the
costume of an unsolved one.

Discovering the branch list also needed care about where to look. Reading only
the entry table would have hidden a branch whose queue had drained but whose
history and lease remained, which is the opposite of useful for a command whose
purpose is to show what exists.

## Impact Assessment
- **Performance**: one additional query per `status` invocation without `--base`,
  over tables measured earlier at a few hundred bytes per row.
- **Security**: none. The command reads and changes nothing, and reaches no
  further than the database it was already given.
- **Maintenance**: `status` now has two output shapes, so a change to the
  per-branch summary has to keep the detailed view coherent with it. Every other
  command keeps one required base branch, which is the thing that stops this
  spreading.

## Risks & Pitfalls
- **Risk**: the overview is mistaken for the detail, and someone concludes a
  queue is empty because the summary did not list its entries.
  **Mitigation**: the summary shows counts, so an empty queue and a queue with
  four waiting entries do not look alike. Accepted beyond that.
- **Risk**: a shared database across repositories makes the repo-scoped listing
  look like the whole picture.
  **Mitigation**: accepted. One database per repository is the default and the
  intent, and the alternative was a git handle per repository plus a case for
  every repository that has since moved.
- **Risk**: someone later adds a multi-branch worker without finding this record.
  **Mitigation**: the rejected option is written out above with its actual cost,
  which is the reason this change got an ADR at all.

## Outcome & Lessons
Pending. To be filled in once the queue has been used on a repository with more
than one base branch, which has not happened yet and is the circumstance that
would show whether one process per branch is genuinely fine in practice.
