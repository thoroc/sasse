#!/usr/bin/env bash
# A migration that has already been committed must never change.
#
# A database that applied the old text will never apply the new one: the runner
# tracks which versions it has run, not what they said. Editing a landed
# migration therefore makes the schema depend on when a queue was created, and
# the two diverge silently. Adding 0005 is always the answer.
#
# With no argument this checks what is staged, which is the pre-commit case.
# Given a base ref it checks the range against it, which is the CI case.
set -euo pipefail

base="${1:-}"

if [ -n "$base" ]; then
    changed=$(git diff --name-only --diff-filter=MD "$base"...HEAD -- 'migrations/*.sql')
    where="between $base and HEAD"
else
    changed=$(git diff --cached --name-only --diff-filter=MD -- 'migrations/*.sql')
    where="in the staged changes"
fi

if [ -z "$changed" ]; then
    exit 0
fi

{
    echo "migrations are append-only, but these are modified or deleted $where:"
    printf '  %s\n' "$changed"
    echo
    echo "A queue that already ran the old text will never run the new one."
    echo "Add a new migration instead."
} >&2

exit 1
