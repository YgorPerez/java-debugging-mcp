#!/usr/bin/env bash
# Fixture matrix for the four Python scripts that decide what CI publishes (TEST-48, #163).
#
#     bash scripts/tests/run.sh              # check every case
#     bash scripts/tests/run.sh --update     # rewrite the expected transcripts, then READ THE DIFF
#
# ## Why these four and not the other scripts
#
# `release-notes.py` builds the release body, which IS the release notes and is the one mitigation
# `docs/toolkit-contract.md` names for five silent downstream failure modes. `sarif-for-code-scanning.py`
# decides what reaches GitHub's security tab and what is withheld. `shard-plan.py` decides which tests run
# in which leg. `test-timings.py` writes `timings.tsv`, which `shard-plan.py` then reads. None of them had
# a test, while `.githooks/test.sh` is a 22-case matrix, `.claude/hooks/pre-bash-guard.test.sh` a 20-case
# one, and `docs_claims.rs` asserts seven of CLAUDE.md's claims against the tree.
#
# ## The cost already paid, which is why release-notes.py leads
#
# Its categoriser did not match the compound `fix(lint)+docs:` form, so **13 commits landed in published
# release notes under "Other Changes" with their type stripped off**. That was found by REL-4 (#147) — by
# the commit-msg hook replaying this repo's own subjects — not by anything testing the script. A dozen
# hand-written subjects would have caught it before it shipped. That subject is the second record in
# `release-notes/history.txt`.
#
# ## A transcript per case, not a pile of small files
#
# Each case is one committed file holding the command, the exit status, stdout and stderr separately, and
# any file the script wrote. Separately, because the split is part of the contract — `shard-plan.py` puts
# names on stdout so a caller can pipe them and the whole report on stderr — and a merged capture would
# let that swap silently. The exit status is in there for the same reason: `--which` on a name in NO shard
# must exit non-zero, and that is precisely the case that otherwise reads as a pass.
#
# ## `--update` exists and is a hazard
#
# DOC-7 (#108) is the record of a generated file that people regenerated without reading. The transcripts
# are small and are meant to be read in the diff; if a change to one is not explainable in the commit
# message, it is a finding rather than a refresh.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1

UPDATE=0
[ "${1:-}" = "--update" ] && UPDATE=1

if ! command -v python3 >/dev/null 2>&1; then
    printf 'SKIP  python3 is not installed, so none of these ran. They GATE in CI.\n'
    exit 1
fi

HERE=scripts/tests
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

# Fold out everything that legitimately differs between two runs of the same input.
#
# Short shas: the release-notes fixture repo is built from empty commits with fixed dates, so its shas ARE
# reproducible today — but that is a property of git's hash algorithm and commit format, not of this test,
# and pinning it would make a future git a failure of the categoriser. The temp directory is the other one.
normalise() {
    sed -E -e "s#$WORK#<work>#g" -e 's/\b[0-9a-f]{7,40}\b/<sha>/g'
}

# check <name> <expected-file> [--artifact <path>] -- <command...>
#
# Runs the command, builds the transcript, and either compares it or rewrites it.
check() {
    local name="$1" expected="$2"
    shift 2
    local artifact=""
    if [ "${1:-}" = "--artifact" ]; then
        artifact="$2"
        shift 2
    fi
    [ "${1:-}" = "--" ] && shift

    local out="$WORK/out" err="$WORK/err" got="$WORK/got" status=0
    "$@" >"$out" 2>"$err" || status=$?

    # Everything goes through `normalise`, INCLUDING the echoed command line — the temp directory appears
    # there too, and normalising only the captured output left an absolute `/tmp/tmp.XXXX` path in three
    # committed transcripts that would have failed on the next run.
    {
        printf '$ %s\n' "$*"
        printf 'exit: %d\n' "$status"
        printf -- '--- stdout\n'
        cat "$out"
        printf -- '--- stderr\n'
        cat "$err"
        if [ -n "$artifact" ]; then
            printf -- '--- artifact: %s\n' "${artifact##*/}"
            # Pretty-printed, because the script writes one long line and a one-line diff tells you
            # nothing about which of a dozen results moved.
            python3 -m json.tool --indent 2 "$artifact" 2>/dev/null ||
                printf '(not valid JSON, or not written)\n'
        fi
    } | normalise >"$got"

    if [ "$UPDATE" -eq 1 ]; then
        cp "$got" "$expected"
        printf 'WROTE %s\n' "$expected"
        return
    fi
    if [ ! -f "$expected" ]; then
        fail=$((fail + 1))
        printf 'FAIL  %s: %s does not exist. Create it with --update, then read it.\n' "$name" "$expected"
        return
    fi
    if diff -u "$expected" "$got" >"$WORK/diff"; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf 'FAIL  %s\n' "$name"
        sed 's/^/      /' <"$WORK/diff"
    fi
}

# ── release-notes.py ────────────────────────────────────────────────────────────
#
# Replayed into a throwaway repo rather than run against this one: the point is to pin what the categoriser
# does to a fixed set of subjects, and this repo's history changes with every commit. `history.txt` is that
# set, one branch of the categoriser per record.
build_release_notes_repo() {
    local repo="$WORK/notes-repo"
    mkdir -p "$repo"
    git -C "$repo" init -q -b main
    git -C "$repo" config user.name "Fixture"
    git -C "$repo" config user.email "fixture@example.invalid"

    local -a lines=()
    local record=""
    flush() {
        [ -z "$record" ] && return
        local subject="${record%%$'\n'*}"
        if [[ "$subject" == @tag\ * ]]; then
            # NOT silenced. `git tag` on a repo with no commits fails, and swallowing that produced a
            # transcript with a narrative and no changelog — indistinguishable from the categoriser
            # matching nothing, which is the failure this whole matrix is about.
            git -C "$repo" tag -a "${subject#@tag }" -m "${subject#@tag }" ||
                { printf 'FATAL fixture repo: could not tag %s\n' "${subject#@tag }" >&2; exit 1; }
        else
            GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z" \
                git -C "$repo" commit -q --allow-empty -m "$record"
        fi
        record=""
    }
    while IFS= read -r line; do
        if [ "$line" = "%%" ]; then
            flush
            continue
        fi
        # A top-level `#` is a comment about the fixture. A body line is indented or follows a subject, and
        # the records here deliberately have no `#`-leading body line so this stays a one-rule format.
        [[ "$line" == \#* ]] && continue
        if [ -z "$record" ]; then
            [ -z "$line" ] && continue
            record="$line"
        else
            record="$record"$'\n'"$line"
        fi
    done <"$HERE/release-notes/history.txt"
    flush
    printf '%s' "$repo"
}

notes_repo="$(build_release_notes_repo)"
notes() {
    # A fixed slug, so the compare link does not depend on whose clone this is.
    ( cd "$notes_repo" && GITHUB_REPOSITORY=owner/repo python3 "$OLDPWD/scripts/release-notes.py" "$@" )
}
check "release-notes: the whole categoriser over one fixed history" \
    "$HERE/release-notes/full-range.expected" -- notes v0.2.0 --since v0.1.0
check "release-notes: a range with nothing in it" \
    "$HERE/release-notes/empty-range.expected" -- notes v0.2.0 --since v0.2.0
# The vocabulary .githooks/commit-msg reads instead of carrying a second copy (REL-4, #147). If this list
# changes, the hook's behaviour changes with it and nothing else says so.
check "release-notes: --list-types, which the commit-msg hook consumes" \
    "$HERE/release-notes/list-types.expected" -- notes --list-types

# ── sarif-for-code-scanning.py ──────────────────────────────────────────────────
sarif() { python3 scripts/sarif-for-code-scanning.py "$@"; }
check "sarif: resolve, re-anchor, ambiguity, and the withholding it exists for" \
    "$HERE/sarif/mixed.expected" --artifact "$WORK/mixed.out.sarif" \
    -- sarif "$HERE/sarif/mixed.in.sarif" "$WORK/mixed.out.sarif"
check "sarif: nothing survives the filter, and the empty upload is the point" \
    "$HERE/sarif/all-notes.expected" --artifact "$WORK/all-notes.out.sarif" \
    -- sarif "$HERE/sarif/all-notes.in.sarif" "$WORK/all-notes.out.sarif"
# The guard that keeps a crashed scan from publishing "all clear": exit 1, and nothing written.
check "sarif: a truncated scan is refused rather than published as empty" \
    "$HERE/sarif/truncated.expected" -- sarif "$HERE/sarif/truncated.in.sarif" "$WORK/truncated.out.sarif"

# ── shard-plan.py ───────────────────────────────────────────────────────────────
plan() { python3 scripts/shard-plan.py --timings "$HERE/shard-plan/timings.tsv" --tests "$HERE/shard-plan/tests.list" "$@"; }
# Both drift directions in one transcript: `g_has_no_recorded_duration` is in the binary and not the
# timings file, `absent_from_the_binary` is the other way round. Neither is fatal and both are named.
check "shard-plan: the split, and both directions of drift" \
    "$HERE/shard-plan/plan.expected" -- plan --plan --shards 2
check "shard-plan: one shard's names on stdout, the report on stderr" \
    "$HERE/shard-plan/shard-1-of-2.expected" -- plan --shard 1/2
# THE CASE THAT OTHERWISE LOOKS LIKE A PASS. `--which` on a name in no shard must exit non-zero: a caller
# reading only stdout sees nothing and concludes the test is unsharded rather than absent.
check "shard-plan: --which for a name that is in NO shard exits non-zero" \
    "$HERE/shard-plan/which-missing.expected" -- plan --which no_such_test
check "shard-plan: --which for a name that is there" \
    "$HERE/shard-plan/which-found.expected" -- plan --which very_slow
# More shards than tests is an empty shard, which is a green run of nothing.
check "shard-plan: more shards than tests is refused" \
    "$HERE/shard-plan/too-many-shards.expected" -- plan --plan --shards 99

# ── test-timings.py ─────────────────────────────────────────────────────────────
timings() { python3 scripts/test-timings.py "$@"; }
check "test-timings: a failed test's duration is reported and labelled" \
    "$HERE/test-timings/panicked.expected" -- timings "$HERE/test-timings/panicked.log"
# `--emit-timings` is what writes mcp-server/tests/timings.tsv, which shard-plan.py then reads. Pinning it
# is what stops a silent change to the file format from reaching the sharding.
check "test-timings: --emit-timings, the input shard-plan.py consumes" \
    "$HERE/test-timings/panicked-emit.expected" -- timings --emit-timings "$HERE/test-timings/panicked.log"
check "test-timings: markdown, which is what the job summaries publish" \
    "$HERE/test-timings/panicked-markdown.expected" -- timings --markdown --label "Fixture" "$HERE/test-timings/panicked.log"
# A log with no durations at all reports that and exits 0. It is an instrument, not a gate: failing the
# suite because the stopwatch broke trades a measurement for a red build.
check "test-timings: no durations in the log is a report, not a failure" \
    "$HERE/test-timings/no-timings.expected" -- timings "$HERE/test-timings/no-timings.log"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
