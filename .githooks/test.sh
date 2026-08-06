#!/usr/bin/env bash
# Test matrix for the git hooks. Run: bash .githooks/test.sh
#
# Same reasoning as .claude/hooks/pre-bash-guard.test.sh, and the same weighting: the must-NOT-fire
# cases are the half that matters. A commit-msg hook that rejects a subject the maintainer actually
# writes is a hook that gets uninstalled the same day, and the check is worth far less than the commit.
#
# The strongest case here is the last one, because it is not a hand-written list of examples: it replays
# every subject in this repo's own history through the hook. That case is what caught both real bugs in
# the first version — `merge:` (10 commits) had no place in the vocabulary, and the compound
# `fix(lint)+docs:` form (13 commits) failed the regex outright.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

pass=0
fail=0
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# want=accept|reject
check_msg() {
    local want="$1" desc="$2" subject="$3"
    printf '%s\n' "$subject" >"$tmp"
    local got
    if .githooks/commit-msg "$tmp" >/dev/null 2>&1; then got=accept; else got=reject; fi
    if [ "$got" = "$want" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf 'FAIL  wanted %s, got %s: %s\n      %s\n' "$want" "$got" "$desc" "$subject"
    fi
}

# ── must accept: the forms this repo actually writes ────────────────────────────
check_msg accept "plain type"                  "docs: a plain subject"
check_msg accept "type with scope"             "fix(docs): a scoped subject"
check_msg accept "breaking marker"             "feat!: a breaking change"
check_msg accept "scope and breaking marker"   "feat(api)!: a scoped break"
check_msg accept "release commit"              "chore(release): 0.19.0"
check_msg accept "compound, 13 in history"     "fix(lint)+docs: two things at once"
check_msg accept "compound, unknown tail type" "docs(dump)+measure: the tail need not be known"
check_msg accept "merge, 10 in history"        "merge: land a series together"
check_msg accept "trailing issue refs"         "ci(deps): something (#141, #152)"

# ── must accept: not ours to judge ──────────────────────────────────────────────
check_msg accept "git merge message"           "Merge branch 'main' into topic"
check_msg accept "git revert message"          "Revert \"fix(docs): something\""
check_msg accept "fixup"                       "fixup! fix(docs): something"
check_msg accept "squash"                      "squash! fix(docs): something"
check_msg accept "empty message"               ""
check_msg accept "comment line only"           "# please enter a commit message"

# ── must reject: the cases the hook exists for ──────────────────────────────────
check_msg reject "near-miss type"              "fixes(trace): a type nobody here uses"
check_msg reject "missing colon"               "chore(ci) no colon here"
check_msg reject "no type at all"              "just a sentence about the change"
check_msg reject "type but no description"     "feat:"
check_msg reject "unknown leading type"        "measure(dump): leading type must be known"

# ── the case that is not a list of examples ─────────────────────────────────────
# Every subject since the fork. The upstream this was forked from (navicore/jdwp-mcp) predates the
# convention, and its 22 commits are excluded by AUTHOR rather than by count or by matching a subject:
# the boundary is the newest commit Ed Sweeney wrote, which needs no number to stay correct as the
# history grows. Every hardcoded count in this repo has rotted at least once, and CLAUDE.md says so
# about shard numbers, ignored-test counts and its own measurements.
fork_tip="$(git log --author='Ed Sweeney' --format='%H' -1)"
if [ -z "$fork_tip" ]; then
    printf 'SKIP  could not locate the upstream commits by author; history replay not run\n'
else
    rejected=0
    while IFS= read -r subject; do
        printf '%s\n' "$subject" >"$tmp"
        .githooks/commit-msg "$tmp" >/dev/null 2>&1 || {
            rejected=$((rejected + 1))
            printf 'FAIL  history subject rejected: %s\n' "$subject"
        }
    done < <(git log --format='%s' "${fork_tip}..HEAD")
    if [ "$rejected" -eq 0 ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
    fi
fi

# ── pre-commit: it must agree with `cargo fmt --check` on the tree as it stands ──
if cargo fmt --all --check >/dev/null 2>&1; then
    if .githooks/pre-commit >/dev/null 2>&1; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf 'FAIL  pre-commit rejected a tree that `cargo fmt --all --check` accepts\n'
    fi
else
    printf 'SKIP  tree is not rustfmt-clean, so pre-commit agreement was not tested\n'
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
