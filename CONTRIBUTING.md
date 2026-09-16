# Contributing

Everything lands on `main` through a pull request. Direct pushes are refused,
including for the repository owner: the `ci` check is required and a push cannot
carry one.

## The loop

```sh
mise run hooks:install        # once per clone
git switch -c feat/the-thing
# work
mise run ci                   # what CI runs: check, then test
git push -u origin HEAD
gh pr create --fill
```

`ci` is the only required check. It depends on `check` and `test`, so requiring
it gates both without having to remember to require each one.

## What the hooks do

`hk install` wires two. Pre-commit fixes and checks the files being committed.
Pre-push adds the test suite, which is deliberately not in pre-commit: a full
suite on every commit is a hook people turn off.

To run the pre-push hook by hand, close its stdin: `hk run pre-push </dev/null`.
A pre-push hook receives the refs being pushed on stdin, so without that it
waits for input forever and looks like a hang.

Neither hook is a substitute for CI. A local pass is not a CI pass: one bug in
this repository passed on macOS and failed on Linux because the two shells
differ in whether they `exec` the final command of a script. If a check can only
run locally, it will eventually only pass locally.

## Merging

Squash or rebase. Merge commits are disabled because `main` requires linear
history, so a merge commit would be refused at merge time and the button would
be a dead end. Branches are deleted on merge.

A branch must be up to date with `main` before it merges. That is the naive
serialisation this project exists to replace, and it is here only because
GitHub's own merge queue is not enabled and sasse is not wired into GitHub.
See `docs/adr/0003-base-branch-overview.md` for the same argument in another
context.

## Commits

Conventional commits. One concern per commit: a fix and the formatting churn it
uncovered belong in separate commits, and so does a lockfile that rode along.

The message explains why, not what. The diff already says what changed; it
cannot say what would have gone wrong otherwise, which is the part worth having
in a year.

## Decisions

A choice with rejected alternatives that someone could reasonably re-propose
gets an ADR under `docs/adr/`, created with the `adr` CLI and scored by `adr
check`. A record whose Options Considered section is empty has recorded nothing:
the value is in why the other paths lost.

Records are numbered `NNNN-slug.md`, chronologically. The CLI writes
`<slug>.md`, so rename it and update the `file` field in `adr-index.toml` to
match; `scripts/check-adr-numbering.sh` will tell you if you forget.

An accepted ADR is immutable except for its status. Supersede rather than edit.

## Migrations

`migrations/*.sql` is append-only, enforced by
`scripts/check-migrations-append-only.sh` in the pre-commit hook and again in CI
against the pull request's base.

A database that applied the old text will never apply the new one, because the
runner records which versions it has run rather than what they said. Editing a
landed migration therefore makes the schema depend on when a queue was created,
and the two diverge silently. Adding a new migration is always the answer.
