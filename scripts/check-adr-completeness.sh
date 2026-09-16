#!/usr/bin/env bash
# A decision record has to have recorded a decision, and this is what says so.
#
# The scoring is vendored from the `adr` CLI that used to own it. That tool's
# repository no longer exists and the only copy of its source is a local bundle,
# so a rule that depended on it was a rule that depended on one unbacked file.
# `hk.pkl` excludes docs/adr/** from markdownlint because those files follow a
# fixed section structure; this is the gate that exclusion assumes.
#
# Five weighted rules, out of 100:
#
#   20  no unfilled <!-- --> placeholders, and at least one section present
#   25  Problem Statement, or Context beneath it, at 30 words or more
#   25  Chosen Solution at 30 words or more
#   20  Rationale at 20 words or more
#   10  Impact Assessment with one entry whose value is not "none"
#
# A section that is more than 60% filler words counts as absent. Two caps then
# apply, the lowest triggered one winning: a thin problem or solution caps the
# score at 60, and three or more placeholders cap it at 70. A record passes at
# 80, or at 95 under --strict.
#
# One deliberate difference from the original: it matched placeholders with
# `<!--[^>]*-->`, so a comment containing a `>` was never seen and never
# counted. This matches the intent instead, so such a record scores lower here
# than it did there.
set -euo pipefail

dir="docs/adr"
threshold=80

usage() {
    echo "usage: $0 [--strict] [--min N] [file...]" >&2
}

files=()
while [ $# -gt 0 ]; do
    case "$1" in
    --strict)
        threshold=95
        shift
        ;;
    --min)
        if [ $# -lt 2 ]; then
            usage
            exit 2
        fi
        threshold="$2"
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    -*)
        usage
        exit 2
        ;;
    *)
        files+=("$1")
        shift
        ;;
    esac
done

if [ ${#files[@]} -eq 0 ]; then
    if [ ! -d "$dir" ]; then
        exit 0
    fi
    while IFS= read -r path; do
        files+=("$path")
    done < <(find "$dir" -maxdepth 1 -name '*.md' | sort)
fi

if [ ${#files[@]} -eq 0 ]; then
    exit 0
fi

score_one() {
    awk -v threshold="$threshold" '
        function strip_comments(s,   out, i, j) {
            out = ""
            while (1) {
                i = index(s, "<!--")
                if (i == 0) { out = out s; break }
                out = out substr(s, 1, i - 1)
                s = substr(s, i + 4)
                j = index(s, "-->")
                if (j == 0) break
                s = substr(s, j + 3)
            }
            return out
        }

        function count_comments(s,   n, i, j) {
            n = 0
            while (1) {
                i = index(s, "<!--")
                if (i == 0) return n
                s = substr(s, i + 4)
                j = index(s, "-->")
                if (j == 0) return n
                n = n + 1
                s = substr(s, j + 3)
            }
        }

        function count_words(s,   parts) {
            gsub(/[ \t\r\n]+/, " ", s)
            sub(/^ /, "", s)
            sub(/ $/, "", s)
            if (s == "") return 0
            return split(s, parts, " ")
        }

        # The highest word count among sections whose heading contains name.
        # The records put "Problem Statement" above an empty line and "Context"
        # beneath it, so the parent heading must not shadow the child that holds
        # the content.
        function best_words(name,   sec, w, best) {
            best = -1
            for (sec in body) {
                if (index(sec, name) > 0) {
                    w = count_words(strip_comments(body[sec]))
                    if (w > best) best = w
                }
            }
            return best
        }

        function best_body(name,   sec, w, best, chosen) {
            best = -1
            chosen = ""
            for (sec in body) {
                if (index(sec, name) > 0) {
                    w = count_words(strip_comments(body[sec]))
                    if (w > best) { best = w; chosen = strip_comments(body[sec]) }
                }
            }
            return chosen
        }

        # More than 60% filler means the section says nothing.
        function is_filler(text,   tokens, n, i, t, hits) {
            n = split(tolower(text), tokens, /[ \t\r\n]+/)
            if (n == 0) return 0
            hits = 0
            for (i = 1; i <= n; i++) {
                t = tokens[i]
                gsub(/^[.,;:!?"'"'"'()]+|[.,;:!?"'"'"'()]+$/, "", t)
                if (t in filler) hits++
            }
            return (hits / n) > 0.6
        }

        BEGIN {
            split("describe decision context placeholder example tbd todo", words, " ")
            for (i in words) filler[words[i]] = 1
            sections = 0
            current = ""
        }

        {
            full = full $0 "\n"
            if ($0 ~ /^## / || $0 ~ /^### /) {
                current = $0
                sub(/^#+[ \t]*/, "", current)
                sub(/[ \t]+$/, "", current)
                if (!(current in body)) sections++
                body[current] = ""
            } else if (current != "") {
                body[current] = body[current] $0 "\n"
            }
        }

        END {
            placeholders = count_comments(full)

            problem = best_words("Problem Statement")
            if (problem < 30) {
                context = best_words("Context")
                if (context > problem) problem = context
            }
            problem_text = best_words("Problem Statement") >= 30 \
                ? best_body("Problem Statement") : best_body("Context")
            solution = best_words("Chosen Solution")
            rationale = best_words("Rationale")

            problem_ok = (problem >= 30) && !is_filler(problem_text)
            solution_ok = (solution >= 30) && !is_filler(best_body("Chosen Solution"))
            rationale_ok = (rationale >= 20) && !is_filler(best_body("Rationale"))

            impact_ok = 0
            for (sec in body) {
                if (index(sec, "Impact") == 0) continue
                n = split(body[sec], lines, "\n")
                for (i = 1; i <= n; i++) {
                    line = lines[i]
                    sub(/^[ \t]+/, "", line)
                    if (line !~ /^-/) continue
                    line = strip_comments(line)
                    pos = 0
                    for (j = length(line); j > 0; j--) {
                        if (substr(line, j, 1) == ":") { pos = j; break }
                    }
                    if (pos == 0) continue
                    value = tolower(substr(line, pos + 1))
                    gsub(/^[ \t]+|[ \t]+$/, "", value)
                    if (value != "" && value != "none") { impact_ok = 1; break }
                }
                if (impact_ok) break
            }

            score = 0
            missing = ""

            if (placeholders == 0 && sections > 0) {
                score += 20
            } else if (placeholders > 0) {
                missing = missing sprintf("    %d unfilled placeholder(s) remain (-20)\n", placeholders)
            } else {
                missing = missing "    no section headings found (-20)\n"
            }

            if (problem_ok) score += 25
            else missing = missing "    Problem Statement / Context is absent, under 30 words, or filler (-25)\n"

            if (solution_ok) score += 25
            else missing = missing "    Chosen Solution is absent, under 30 words, or filler (-25)\n"

            if (rationale_ok) score += 20
            else missing = missing "    Rationale is absent, under 20 words, or filler (-20)\n"

            if (impact_ok) score += 10
            else missing = missing "    Impact Assessment has no entry other than \"none\" (-10)\n"

            cap = 100
            capped = ""
            if (!problem_ok && cap > 60) {
                cap = 60
                capped = "a thin Problem Statement caps the score at 60"
            }
            if (!solution_ok && cap > 60) {
                cap = 60
                capped = "a thin Chosen Solution caps the score at 60"
            }
            if (placeholders >= 3 && cap > 70) {
                cap = 70
                capped = "three or more placeholders cap the score at 70"
            }
            if (score > cap) score = cap

            printf "%d\n", score
            printf "%s", missing
            if (capped != "") printf "    %s\n", capped
        }
    ' "$1"
}

problems=0

for path in "${files[@]}"; do
    if [ ! -f "$path" ]; then
        echo "no such record: $path" >&2
        problems=$((problems + 1))
        continue
    fi

    report=$(score_one "$path")
    score=$(printf '%s\n' "$report" | head -1)
    detail=$(printf '%s\n' "$report" | tail -n +2)

    if [ "$score" -lt "$threshold" ]; then
        echo "$path scores $score/100, below $threshold" >&2
        if [ -n "$detail" ]; then
            printf '%s\n' "$detail" >&2
        fi
        problems=$((problems + 1))
    fi
done

if [ "$problems" -gt 0 ]; then
    echo >&2
    echo "$problems record(s) have not recorded a decision." >&2
    echo "A section that is present but empty has recorded nothing: the value is" >&2
    echo "in why the other paths lost, not in the heading being there." >&2
    exit 1
fi
