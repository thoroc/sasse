# What sasse takes from task-spooler

Source read: `ts-1.0.4` (Lluís Batlle i Rossell), 5,501 lines of C across 18
files, copyright range 2007 to 2013. Read in full: `PROTOCOL`, `OBJECTIVES`,
`TRICKS`, `buglist.bug`, `teststartrace.sh`, `server_start.c`, `env.c`,
`execute.c`. Read selectively: `jobs.c` (1,541 lines), `client.c`, `main.h`.

`ts` is a single-machine job spooler, not a merge queue. It has no concept of
git, of a base branch, or of testing a candidate before accepting it. So nothing
transfers as code. What transfers is a set of decisions its author got right,
and a smaller set of traps he documented himself.

## Salvage

### 1. The client executes the job, the server only schedules

This is the load-bearing design decision and it is not obvious from the outside.

`PROTOCOL` records the exchange: the client sends `NEWJOB` and blocks; the server
replies `NEWJOB_OK`; when the job reaches the front, the server sends `RUNJOB`;
the client then forks and execs the command itself (`client.c:137` dispatching to
`run_job` at `client.c:152`, which forks at `execute.c:270`); the client reports
`ENDJOB` with the result. The server never execs anything.

That is the whole explanation for the claim on the project page that "the tasks
are run in the correct context (that of enqueue)". There is no environment
serialisation, because the process that runs the job *is* the process that was
sitting in the user's shell. Note that `env.c` is not this mechanism: it only
implements the optional `TS_ENV` hook, which runs a command and captures its
stdout as extra environment.

**Verdict: reject the mechanism, keep the requirement it reveals.** A merge
queue must do the opposite. The gate has to run in a known checkout with a
controlled environment, or the result is not reproducible and the queue is
worthless. But the insight still lands: whatever identifies the request must be
captured at enqueue time, because by the time the batch runs, the developer's
shell has moved on and their working tree has changed. That is invariant 3, and
`ts` is independent confirmation that it belongs at enqueue rather than at run.

### 2. Winning a bind is how you elect a single server

`ensure_server_up()` in `server_start.c`:

1. Try to connect to the socket. Success means a server exists, so attach.
2. On `ENOENT` or `ECONNREFUSED`, try to `bind()` and `listen()`.
3. Whoever wins the bind forks the server and waits on a pipe for it to signal
   readiness. Everyone who loses falls straight through.
4. Both winner and losers then retry the connect exactly once. A second failure
   is fatal.

The `bind()` is the mutex. No lock file, no pid file, no retry loop.

There is a dedicated stress test for it, `teststartrace.sh`: 50 rounds of 5
concurrent clients racing to start the server.

**Verdict: salvage directly, with a different primitive.** sasse needs exactly
one worker per (repo, base branch), and the shape is identical: attempt an atomic
exclusive acquire, and have the loser attach rather than error. Use the database
rather than a socket, either `BEGIN IMMEDIATE` against a `worker_lease` row or an
`O_EXCL` lock file carrying the pid. Port the stress test too. This is the class
of bug that appears only under concurrency, and the author clearly learned that
the hard way.

### 3. Recover a stale lock carefully, not eagerly

On `ECONNREFUSED`, `ts` unlinks the socket and retries. Two details make that
safe rather than reckless:

- `is_path_unixsocket()` confirms the path really is a socket before unlinking,
  so a regular file sitting at that path is an error, not something to delete.
- `try_check_ownership()` refuses a socket owned by another uid, and is
  deliberately skipped when `TS_SOCKET` was set, because the user may have
  intended a shared queue.

`buglist.bug` entry 28 is the author filing the underlying security problem
against himself, citing the `kdesu` password-caching hole as the precedent for
trusting a socket in a world-writable directory.

**Verdict: salvage the pattern.** Stale lease recovery in sasse needs the same
discipline: confirm the holder is actually dead before stealing the lease, and
never unlink a path whose type has not been checked. The lease lives next to the
queue database, not in a shared temp directory.

### 4. `SKIPPED` is not `FINISHED`

`main.h` declares five job states: `QUEUED`, `RUNNING`, `FINISHED`, `SKIPPED`,
`HOLDING_CLIENT`. `SKIPPED` is reached at `client.c:141`: when a job depends on a
predecessor that exited nonzero, the client runs nothing at all, sets
`skipped = 1`, and reports a result anyway. The job still gets a state and a row.

**Verdict: salvage the state, and it exposes a gap in the current schema.** sasse
today collapses this into `evicted`. That is wrong. An entry that was never gated
because a batch-mate failed is categorically different from an entry that failed
its own gate, and the difference is load-bearing for invariant 4: a skipped entry
should requeue without cost, whereas a failed one must burn an attempt against
its retry budget. Collapsing them means one flaky branch can exhaust the retry
budget of every branch batched alongside it.

**Applied.** Recorded as `candidate_entry.outcome`
(`pending` / `passed` / `culprit` / `skipped`) rather than as an entry state,
because the verdict belongs to the (entry, candidate) pair: an entry joins
several candidates over its life and an entry-level column would be lost on
requeue. `Outcome::burns_attempt` is the rule that only the culprit pays.

### 5. Bounded queue with a distinct full signal

`s_newjob` admits a job as `QUEUED` only while `count_not_finished_jobs() <
max_jobs`; beyond that it becomes `HOLDING_CLIENT`, and `wake_hold_client()`
promotes one when capacity frees. `EXITCODE_QUEUE_FULL = 2` is a distinct exit
code, and `TS_MAXCONN` caps connections at server start and cannot be changed
afterwards.

**Verdict: salvage, low priority.** A bounded queue length plus a distinct
"queue full" exit code is better than unbounded growth, and the separate exit
code lets a wrapper script react rather than parse output.

### 6. Results outlive the queue

Job output goes to a persistent file whose path is recorded on the job, and the
feature list is explicit that result files are never removed, "so they can be
reached even after we've lost the ts task list". `TS_MAXFINISHED` bounds how many
finished records are retained.

**Verdict: salvage.** `run.log_path` already points this way. Adopt the explicit
rule that the log outlives the row, and add a retention bound so finished history
does not grow without limit.

## Reject

- **The client/server socket protocol.** `struct msg` is a C struct containing a
  union, written straight onto a socket and guarded by `PROTOCOL_VERSION = 730`.
  Not portable across architectures, and `PROTOCOL` is itself headed "[Totally
  outdated document]". sasse's database is already both the state store and the
  coordination point, which is strictly better on the axis that matters here: it
  survives a reboot, and the `ts` in-memory job list does not.
- **The intrusive linked list.** `struct Job` carries a `next` pointer, and
  `job_finished()` hand-walks the list to find the node pointing at the finished
  job so it can re-splice around it. Roughly sixty lines to do what one `DELETE`
  does.
- **Global mutable state.** `busy_slots`, `max_slots`, `jobids`,
  `last_errorlevel` and `last_finished_jobid` are file-scope globals, and the
  code concedes the invariant can break: "busy_slots may be bigger than the
  maximum slots, if the user was running many jobs, and suddenly trimmed the
  maximum slots down." The `CHECK` constraints in the schema exist precisely so
  sasse cannot reach that situation.
- **Slot accounting.** The `free_slots` and `num_slots` arithmetic in
  `next_run_job()` exists to run several jobs concurrently. sasse is deliberately
  serial, so it is dead weight.
- **Mail, gzip, and output tailing.** Out of scope.

## Traps the author documented

- **`buglist.bug` entry 19.** Killing the wrong pid inside the job's process tree
  hung the queue permanently; neither `ts -C` nor anything short of `ts -K`
  recovered it. The lesson for sasse is that the worker lease must be recoverable
  without a full reset, which means an expiry plus a liveness check, not a lock
  held for the natural lifetime of a process.
- **Relationships held in memory by id go stale.** `next_run_job()` treats a
  dependency on a job that has vanished as satisfied, and `s_newjob` has to warn
  about a job "suddenly non existent in the queue". Foreign keys remove this
  class of bug entirely.
- **`buglist.bug` entry 29.** `recv()` returning `ECONNRESET` when a client died
  without sending `ENDJOB`, cause never established. The equivalent for sasse is
  a worker killed mid-gate, and it needs a defined answer rather than an open
  bug: the candidate is marked `superseded` on lease expiry and its entries
  requeue.

## Net

One design insight worth having (capture context at enqueue), one mechanism worth
porting (atomic acquire with attach-on-loss, plus its stress test), one concrete
schema gap found (`skipped` distinct from `evicted`), and a set of traps with the
author's own notes on how they bit him. No code transfers.
