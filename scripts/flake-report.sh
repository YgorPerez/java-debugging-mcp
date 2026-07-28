#!/usr/bin/env bash
#
# Harvest the flake evidence CI has already produced, and say which of it is new.
#
# The matrix runs the integration suite on three JDKs per push, so roughly three legs accumulate per
# commit, on hardware and at a parallelism no developer box reproduces. That is the cheapest flake
# instrument this project has and it was being read by hand: TEST-23 (#64) and TEST-24 (#65) were both
# found by someone happening to notice two red runs in `gh run list`, days after they happened. Two
# failures at ~1-in-24 each sat there unnoticed, which is the argument for a command.
#
# It also fixes the part hand-counting gets wrong. Every rate in this project's issues needs a
# denominator, and "24 legs" was previously counted by eye off a list. Here the denominator is the number
# of legs actually *scanned*, printed whether or not anything failed, because a numerator with a guessed
# denominator is not a rate.
#
# THREE DISTINCTIONS IT REFUSES TO BLUR
#
#   1. A failed job is not a failed test. A compile error, a runner dying, or a cancelled run all fail a
#      job while running no tests at all — and a count that lumps them in is how an arm comes to report
#      "8 failures in 40" that were really the author's own mid-flight compile errors (that happened, in
#      this repo, to me). Jobs whose logs contain no `... FAILED` line are reported separately as
#      NON-TEST FAILURES, never as flakes.
#   2. A known flake is not a new one. Every failing test name is looked up against the issue tracker, so
#      output says `#64` or says `UNFILED` — the second being the only line here worth interrupting
#      anyone for. A candidate from `gh issue list --search` is confirmed against the issue's own text
#      before it is accepted, because that search tokenises and will return issues that merely share
#      words. The error is deliberately asymmetric: an unconfirmed match is reported UNFILED, since a
#      false UNFILED costs a reader half a minute and a false attribution hides a new flake entirely.
#   3. Absent evidence is not absence of failure. GitHub expires run logs, so a run whose log cannot be
#      read is counted and named under UNREADABLE rather than treated as a pass. Silence about a run this
#      tool could not open would read exactly like a clean one.
#
# Requires `gh` authenticated, and `jq`.
#
# Usage:
#   scripts/flake-report.sh                 # the last 30 pushes' worth of `tests` runs
#   scripts/flake-report.sh --limit 60      # look further back (logs expire, so older is thinner)
#   scripts/flake-report.sh --workflow ci   # a differently-named workflow
#
set -uo pipefail

# Rates are printed with a '.' whatever the shell's locale is. Under pt_BR awk emits "2,1%", which is
# wrong in every issue comment this output gets pasted into.
export LC_NUMERIC=C

LIMIT=30
WORKFLOW=tests

while [ $# -gt 0 ]; do
  case "$1" in
    --limit) LIMIT="${2:?--limit needs a number}"; shift 2 ;;
    --workflow) WORKFLOW="${2:?--workflow needs a name}"; shift 2 ;;
    -h|--help) sed -n '2,34p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1 (try --help)" >&2; exit 2 ;;
  esac
done

for tool in gh jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "flake-report needs $tool on PATH" >&2; exit 2; }
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "Scanning the last $LIMIT '$WORKFLOW' runs…"

# Completed push runs only. A run still in progress has legs that have not reported, and counting those as
# passes would inflate the denominator with results that do not exist yet.
if ! gh run list --workflow "$WORKFLOW" --limit "$LIMIT" \
      --json databaseId,headSha,status,conclusion,event,createdAt >"$WORK/runs.json" 2>"$WORK/err"; then
  echo "could not list runs: $(cat "$WORK/err")" >&2
  exit 1
fi

jq -r '.[] | select(.status=="completed" and .event=="push")
        | [.databaseId, (.headSha[0:7]), .conclusion, .createdAt] | @tsv' \
  "$WORK/runs.json" >"$WORK/completed.tsv"

runs=0; legs=0; green_legs=0; failed_jobs=0; unreadable=0
: >"$WORK/failures.tsv"   # test <TAB> sha <TAB> leg
: >"$WORK/nontest.tsv"    # sha <TAB> leg
: >"$WORK/messages.tsv"   # test <TAB> first message seen

while IFS=$'\t' read -r id sha conclusion created; do
  [ -n "${id:-}" ] || continue
  runs=$((runs + 1))

  if ! gh run view "$id" --json jobs >"$WORK/jobs.json" 2>/dev/null; then
    unreadable=$((unreadable + 1))
    echo "  ! run $sha: job list unreadable"
    continue
  fi

  # One line per job. Legs are what the rate is per: a job that ran the suite.
  while IFS=$'\t' read -r job_name job_conclusion; do
    [ -n "${job_name:-}" ] || continue
    legs=$((legs + 1))
    if [ "$job_conclusion" = "success" ]; then
      green_legs=$((green_legs + 1))
      continue
    fi
    [ "$job_conclusion" = "failure" ] || continue   # skipped/cancelled are not evidence either way
    failed_jobs=$((failed_jobs + 1))

    if ! gh run view "$id" --log-failed >"$WORK/log.all" 2>/dev/null || [ ! -s "$WORK/log.all" ]; then
      unreadable=$((unreadable + 1))
      echo "  ! run $sha leg '$job_name': failed, log unreadable (expired?)"
      continue
    fi
    # `--log-failed` returns every failed job in the run, and each line is prefixed with its job name.
    # Narrowing to this leg is what stops a run with two failed legs from crediting each with the other's
    # failures — which would double every count and quietly halve the accuracy of the whole report.
    grep -F "$job_name" "$WORK/log.all" >"$WORK/log" || cp "$WORK/log.all" "$WORK/log"

    # `test <name> ... FAILED` is cargo's own line, so this cannot be fooled by a test whose *output*
    # mentions another test's name.
    grep -oE 'test [a-zA-Z0-9_:]+ \.\.\. FAILED' "$WORK/log" \
      | sed -E 's/^test (.*) \.\.\. FAILED$/\1/' | sort -u >"$WORK/named" || true

    if [ ! -s "$WORK/named" ]; then
      printf '%s\t%s\n' "$sha" "$job_name" >>"$WORK/nontest.tsv"
      continue
    fi

    while read -r test_name; do
      [ -n "$test_name" ] || continue
      printf '%s\t%s\t%s\n' "$test_name" "$sha" "$job_name" >>"$WORK/failures.tsv"
      # The assertion, not just its file:line. `panicked at <file>:<line>:` is followed by the message,
      # and the message is the whole reason these issues ask for a captured failure — a report that
      # stopped at the location would send every reader to fetch the log by hand.
      #
      # Each log line is `<job name>\t<step>\t<ISO timestamp> <text>`, and the job name contains spaces
      # ("Integration (JDK 17)"), so the prefix has to be matched through the timestamp rather than by
      # counting non-space fields — which is what the first attempt did, and it printed the job name as
      # the message.
      if ! cut -f1 "$WORK/messages.tsv" | grep -qxF "$test_name"; then
        msg="$(awk -v t="$test_name" '
                 index($0, "panicked at") && index($0, t) { hit = 3 }
                 hit > 0 {
                   line = $0
                   sub(/^.*[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z[ ]?/, "", line)
                   gsub(/^[ \t]+|[ \t]+$/, "", line)
                   if (line != "") out = (out == "" ? line : out " | " line)
                   if (--hit == 0) { print out; exit }
                 }' "$WORK/log")"
        [ -n "$msg" ] && printf '%s\t%s\n' "$test_name" "$msg" >>"$WORK/messages.tsv"
      fi
    done <"$WORK/named"
  done < <(jq -r '.jobs[] | [.name, .conclusion] | @tsv' "$WORK/jobs.json")
done <"$WORK/completed.tsv"

echo
echo "=============================================================="
echo " scanned: $runs completed push run(s), $legs leg(s)"
echo " green legs: $green_legs   failed jobs: $failed_jobs   unreadable: $unreadable"
echo "=============================================================="

if [ "$unreadable" -gt 0 ]; then
  echo
  echo "NOTE: $unreadable job(s) could not be read. They are NOT counted as passes — a run this tool"
  echo "      could not open must not read like a clean one. Rates below are lower bounds."
fi

if [ -s "$WORK/nontest.tsv" ]; then
  echo
  echo "NON-TEST FAILURES (a job failed while running no tests — compile error, runner, cancellation)."
  echo "These are not flakes and must not be counted as any test's failure:"
  sort -u "$WORK/nontest.tsv" | while IFS=$'\t' read -r sha leg; do
    echo "  $sha  $leg"
  done
fi

if [ ! -s "$WORK/failures.tsv" ]; then
  echo
  echo "No failing tests in the scanned window."
  [ "$legs" -lt 24 ] && echo "With only $legs leg(s), that is weak evidence about a ~4%-per-leg flake."
  exit 0
fi

echo
echo "FAILING TESTS, most frequent first (out of $legs leg(s) scanned):"
echo

cut -f1 "$WORK/failures.tsv" | sort | uniq -c | sort -rn | while read -r count test_name; do
  legs_hit="$(awk -F'\t' -v t="$test_name" '$1==t {print $3}' "$WORK/failures.tsv" | sort -u | paste -sd', ' -)"
  shas="$(awk -F'\t' -v t="$test_name" '$1==t {print $2}' "$WORK/failures.tsv" | sort -u | paste -sd' ' -)"

  # Which issue owns this? Searched rather than hardcoded, so a newly filed issue is picked up with no
  # edit here, and an unfiled flake cannot hide behind a stale table.
  #
  # The match is then CONFIRMED against the issue's own text, because GitHub's search tokenises: a query
  # for `a_duplicated_hit_is_buffered_twice` can come back with an issue that merely shares words with it.
  # Accepting that would label a genuinely new flake as already-filed, which is the one mistake this
  # report exists to prevent — so a candidate that does not literally contain the test name is discarded
  # and the test is reported UNFILED.
  issue=""
  for cand in $(gh issue list --state all --limit 100 --search "$test_name" \
                  --json number --jq '.[].number' 2>/dev/null); do
    if gh issue view "$cand" --json body,comments \
         --jq '[.body, (.comments[]?.body // "")] | join("\n")' 2>/dev/null \
         | grep -qF "$test_name"; then
      state="$(gh issue view "$cand" --json state --jq '.state | ascii_downcase' 2>/dev/null)"
      issue="#$cand (${state:-unknown})"
      break
    fi
  done
  [ -n "$issue" ] || issue="UNFILED — no issue's text contains this test name"

  rate=""
  if [ "$legs" -gt 0 ]; then
    rate="$(awk -v c="$count" -v l="$legs" 'BEGIN{printf "%.1f%%", (c*100)/l}')"
  fi

  echo "  $test_name"
  echo "      $count failure(s) / $legs leg(s) = $rate    tracker: $issue"
  echo "      legs: $legs_hit"
  echo "      commits: $shas"
  msg="$(awk -F'\t' -v t="$test_name" '$1==t {print $2; exit}' "$WORK/messages.tsv")"
  [ -n "$msg" ] && echo "      first message: $msg"
  echo
done

echo "A rate here is per *leg*, not per push: one push is three legs, so a 4%-per-leg flake fires on"
echo "roughly one push in eight. Anything marked UNFILED is the line worth acting on."
