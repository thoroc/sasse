#!/usr/bin/env bash
# Decision records are numbered, and the index has to agree with the filenames.
#
# The `adr` CLI has no notion of a number: `adr create` writes `<slug>.md`. So a
# new record arrives unnumbered and this is what notices, rather than a
# convention that quietly decays. Renaming the file and updating the `file`
# field in the index is the fix; the CLI is filename-agnostic once they match.
#
# Numbers need only be unique, not contiguous. A gap is what superseding or
# abandoning a record leaves behind, and closing it would renumber decisions
# that other documents already cite.
set -euo pipefail

dir="docs/adr"
index="$dir/adr-index.toml"
problems=0

if [ ! -d "$dir" ]; then
    exit 0
fi

# Every record carries a three-digit prefix.
while IFS= read -r path; do
    name=$(basename "$path")
    if ! printf '%s' "$name" | grep -qE '^[0-9]{3}-.+\.md$'; then
        echo "not numbered: $path" >&2
        echo "  rename it to NNN-$name, next number after the highest in $dir" >&2
        problems=$((problems + 1))
    fi
done < <(find "$dir" -maxdepth 1 -name '*.md' | sort)

# No two records share a number.
duplicates=$(find "$dir" -maxdepth 1 -name '[0-9][0-9][0-9]-*.md' -exec basename {} \; |
    cut -c1-3 | sort | uniq -d)
if [ -n "$duplicates" ]; then
    echo "duplicate numbers: $(printf '%s' "$duplicates" | tr '\n' ' ')" >&2
    problems=$((problems + 1))
fi

# The index points at files that exist. A record the index cannot find is one
# `adr check` will refuse to score.
if [ -f "$index" ]; then
    while IFS= read -r file; do
        if [ ! -f "$dir/$file" ]; then
            echo "index names a missing file: $file" >&2
            echo "  update its 'file' field in $index" >&2
            problems=$((problems + 1))
        fi
    done < <(grep -oE '^[[:space:]]*file = "[^"]+"' "$index" | sed -E 's/.*"(.*)"/\1/')
fi

if [ "$problems" -gt 0 ]; then
    echo >&2
    echo "$problems problem(s) with the decision records." >&2
    exit 1
fi
