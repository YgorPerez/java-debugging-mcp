#!/usr/bin/env bash
#
# Run the MCP-level integration tests: the real `jdwp-mcp` binary driven over JSON-RPC against real
# probe JVMs (mcp-server/tests/mcp_integration.rs). Each test compiles and launches its own probe
# from examples/probes/ and reaps it afterwards, so there are no manual steps.
#
# Requires a JDK (javac + java). Without one every test prints SKIP and *passes*, so a green run on a
# JDK-less machine proves nothing. That used to be left to the reader to notice, and it was missed: on
# Windows `Jdk::find` looked for an extensionless `java`, never found it, and the whole suite reported ok
# in 0.00s while launching no JVMs at all. The check below is now a hard failure rather than a warning,
# matching the same guard in scripts/coverage.sh — a suite that ran nothing must not exit 0.
#
# A third incident of the same family, and the reason every run now prints one `JDK in use:` line. The
# harness used to try `JAVA_HOME`, then `PATH`, then a snap-installed IntelliJ runtime, and take the first
# one that worked. `JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64` is a JRE with no `javac`, so it was
# discarded in silence and the suite ran the snap JBR — JDK 25 — while being reported as "green on JDK 21"
# for several hours. Two changes, and both are needed: an unusable `JAVA_HOME` is now a hard failure in
# `Jdk::find` rather than a fallthrough, and every run SAYS which JDK it used. The second is the one that
# would have caught it, because the first only helps once you already suspect something (TEST-18, #52).
#
# Usage:
#   scripts/integration-test.sh                     # all of them
#   scripts/integration-test.sh force_return        # only tests whose name contains this
#   scripts/integration-test.sh --test-threads=1    # serial, easier to read when debugging
#   scripts/integration-test.sh --shard 1/2         # half the suite, split by MEASURED duration
#
# Arguments go straight to libtest — this script already supplies the `--`, so do NOT pass another one. The
# single exception is `--shard N/M` (TEST-29, #106), which this script interprets and libtest has never heard
# of; it is refused alongside a name filter rather than silently intersected with one.
# `… -- --test-threads=1` makes libtest read the bare `--` as a test-name FILTER, which matches nothing:
# "0 passed; 47 filtered out", exit 0, and a run that looks fine having executed nothing. The usage line
# above said exactly that until it was caught doing it.
#
# The `--test mcp_integration` scope keeps the output to these tests and skips rebuilding the other
# harnesses; `cargo test -- --ignored` also works if you want everything.
#
# ---
#
# ## Why this builds and runs in two steps instead of just calling `cargo test` (TEST-26, #103)
#
# Every run now reports **per-test durations** and a ranked list of the slowest tests, because the suite
# used to report one aggregate number and nothing said which tests were in it. That number came out of
# libtest's own `--report-time`, which is nightly-gated behind `-Z unstable-options` and reachable on a
# stable toolchain only with `RUSTC_BOOTSTRAP=1` — the documented, unsupported bypass.
#
# **That variable must never be visible to `cargo`, and this was measured rather than assumed.** Cargo
# hashes it into the build fingerprint: setting it on a `cargo test` invocation recompiled the whole
# workspace (14 s locally), and alternating between a run with it and a run without it would recompile
# every time. Worse than the cost, the compile would then be running under a flag that makes nightly-only
# features build silently on a stable toolchain, so a nightly dependency could arrive without anything
# saying so.
#
# So the build happens under `cargo test --no-run` with a clean environment, and the variable is set only
# on the test binary, where the only thing that reads it is libtest's argument check. `cargo`'s own
# environment reaches the tests through compile-time `env!` — `CARGO_BIN_EXE_jdwp-mcp`,
# `CARGO_MANIFEST_DIR` — so those are baked into the binary and running it directly loses nothing. That
# was verified by running the full suite this way: 117 passed, the same set as before.
#
# `cargo-nextest` was the other candidate and was rejected; docs/adr/0024 has the reasoning, the short
# version being that it schedules a process per test and #103 puts a scheduling change out of scope.
set -euo pipefail

cd "$(dirname "$0")/.."

LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

# `--shard N/M` is the ONE argument this script interprets rather than forwarding (TEST-29, #106). Everything
# else still goes straight to libtest, which is the contract the usage note above depends on — so it is pulled
# out of "$@" here and the remainder is passed on untouched.
SHARD=""
ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --shard)
      [ $# -ge 2 ] || {
        echo "error: --shard wants N/M, e.g. --shard 1/2" >&2
        exit 1
      }
      SHARD="$2"
      shift 2
      ;;
    --shard=*)
      SHARD="${1#--shard=}"
      shift
      ;;
    *)
      ARGS+=("$1")
      shift
      ;;
  esac
done
set -- ${ARGS[@]+"${ARGS[@]}"}

# --------------------------------------------------------------------------------------------------
# Concurrency: OVERSUBSCRIBE, because this suite waits far more than it computes (TEST-32).
#
# libtest defaults to `available_parallelism()`, which is the right default for CPU-bound tests and the
# wrong one here. Almost every test in this suite spends its time waiting on a probe JVM — to start, to
# load a class, to reach a tick — so at one thread per core the cores sit idle. Measured on 4 vCPU
# (`taskset -c 0-3`), 124 tests, 540 s of test time:
#
#     threads   wall clock   concurrency
#           4        139.1s          3.8x   <- libtest's default, what this used to run
#           8         87.7s          6.3x
#          16         63.8s         10.2x
#
# 10.2x concurrent on FOUR cores. Sixteen cores continue the curve: 16 -> 56.9s, 24 -> 50.1s, 40 -> 45.6s.
#
# Worth stating why the neighbouring repos do not do this and are not wrong: `b2c-next`'s vitest takes the
# default of one worker per core and `~/html/infotravel-doc` caps Playwright at `workers: 4` on CI. Both are
# CPU-bound JS. Copying their NUMBER here would be copying the wrong half of the idea; the transferable part
# is "do not accept a default that assumes work you do not have".
#
# 4x cores, capped at 40 — both ends measured (4 vCPU -> 16, this workspace's 16 cores -> 40). The cap is
# there because the gain is flattening by then (24 -> 40 buys 4.5 s) while each thread is another probe JVM
# competing for memory, and a machine with less RAM than cores*300MB would start swapping rather than
# finishing sooner. `JDWP_TEST_THREADS` overrides it; an explicit `--test-threads` on the command line
# still beats both, which is what keeps `--test-threads=1` usable for reading a failure.
# --------------------------------------------------------------------------------------------------
if [[ " $* " != *" --test-threads"* ]]; then
  if [ -n "${JDWP_TEST_THREADS:-}" ]; then
    threads=$JDWP_TEST_THREADS
  else
    cores=$(nproc 2>/dev/null || echo 4)
    threads=$((cores * 4))
    [ "$threads" -gt 40 ] && threads=40
  fi
  echo "Test threads: $threads (oversubscribing ${cores:-?} core(s) — this suite waits on JVMs more than it computes; JDWP_TEST_THREADS overrides)"
  set -- "$@" "--test-threads=$threads"
fi

# Build with a clean environment — see the note above on RUSTC_BOOTSTRAP and the fingerprint. Build output
# goes to the terminal, not into $LOG: the guards below grep for test results, and a `Compiling` line has
# never been one.
cargo test --test mcp_integration --no-run

# `"executable":"…"` from cargo's JSON, rather than parsing the human-readable `Executable` line. Nothing
# is compiled here — the build above already did it — so this is a metadata query.
BIN="$(cargo test --test mcp_integration --no-run --message-format=json 2>/dev/null |
  sed -n 's/.*"executable":"\([^"]*mcp_integration[^"]*\)".*/\1/p' | tail -1)"

if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "error: could not locate the mcp_integration test binary; cargo reported no executable." >&2
  echo "       This is a build failure wearing a different hat — the run cannot proceed." >&2
  exit 1
fi

# Nightly-gated flags, and the reason for the retry below. libtest rejects an unaccepted flag *before*
# running any test and exits 101, so a toolchain that removes the RUSTC_BOOTSTRAP bypass costs one wasted
# fraction of a second rather than a wasted suite — and the timing report degrades to a note instead of
# failing the gate. Timings are an instrument; the three guards further down are the gate.
TIMING=(-Z unstable-options --report-time)

# Sharding (TEST-29, #106). libtest has no shard flag, so a shard is expressed as the thing libtest does have:
# an explicit list of names under `--exact`. `scripts/shard-plan.py` picks the names by **measured** duration
# from `mcp-server/tests/timings.tsv`, which matters rather than being a nicety — the four resume-honesty
# tests are 29% of the suite's test time and the slowest single test is 70 s, so a split by name has a real
# chance of putting the heavy ones together and making one shard the whole wall clock.
#
# The plan's report goes to stderr and is deliberately not suppressed: it names any test with no recorded
# duration, which is the one way this can quietly go wrong — a new slow test charged the median, landing in a
# shard, and making one leg longer for a reason nobody can see.
SELECTION=()
if [ -n "$SHARD" ]; then
  if [ "$#" -gt 0 ]; then
    echo "error: --shard and a test-name filter together would silently intersect: the shard plan covers" >&2
    echo "       every test, and filtering it afterwards leaves a shard that is neither the whole shard" >&2
    echo "       nor the whole filter. Pick one." >&2
    exit 1
  fi
  LIST="$(mktemp)"
  trap 'rm -f "$LOG" "$LIST"' EXIT
  "$BIN" --ignored --list >"$LIST"
  mapfile -t SELECTION < <(scripts/shard-plan.py --shard "$SHARD" --tests "$LIST")
  if [ "${#SELECTION[@]}" -eq 0 ]; then
    echo "" >&2
    echo "error: shard $SHARD selected no tests, so this run would be a green run of nothing." >&2
    exit 1
  fi
  echo "shard $SHARD: ${#SELECTION[@]} tests selected by measured duration"
  SELECTION=(--exact "${SELECTION[@]}")
fi

# `2>&1` into the tee, which it did not used to be, and the omission hid the very thing the retry below
# looks for: libtest writes its argument refusals to STDERR, so a log built from stdout alone could never
# contain one and the fallback could never fire. Found by defeating it on purpose rather than by a toolchain
# ever refusing. Everything else in the log is unaffected — libtest prints test results and the harness
# prints `SKIP`/`JDK in use:` on stdout — so the merge only adds what the guards were missing.
run_suite() {
  set +e
  # pipefail is set, so ${PIPESTATUS[0]} is read before anything can overwrite it.
  RUSTC_BOOTSTRAP=1 "$BIN" --ignored --nocapture "${TIMING[@]}" \
    ${SELECTION[@]+"${SELECTION[@]}"} "$@" 2>&1 | tee "$LOG"
  status=${PIPESTATUS[0]}
  set -e
}

refused=0
run_suite "$@"

# Matched on the shared tail rather than the whole sentence, because libtest has TWO refusals here and
# only grepping for one of them was caught by defeating the fallback deliberately: `-Z` without the bypass
# says "the option `Z` is only accepted on the nightly compiler", while `--report-time` without `-Z` says
# "The \"report-time\" flag is only accepted on the nightly compiler with -Z unstable-options". A future
# toolchain could produce either.
if [ "$status" -ne 0 ] && grep -q 'is only accepted on the nightly compiler' "$LOG"; then
  echo "" >&2
  echo "note: libtest refused --report-time, so this run has no per-test timings. Re-running without" >&2
  echo "      them — the suite still gates, only the measurement is lost (see TEST-26, #103)." >&2
  TIMING=()
  refused=1
  run_suite "$@"
fi

if grep -q 'SKIP .*no JDK found' "$LOG"; then
  echo "" >&2
  echo "error: tests SKIPPED — no JDK found, so nothing actually ran. Set JAVA_HOME or put javac on" >&2
  echo "       PATH. A skip is not a pass; failing instead of reporting a green run of nothing." >&2
  grep 'SKIP .*no JDK found' "$LOG" >&2
  exit 1
fi

# The other way to get a green run of nothing: every test filtered out. libtest reports that as
# "0 passed" and exits 0, which is indistinguishable from success at a glance — a filter typo, or the
# stray `--` described above, both land here. An empty selection is a failed request, not a pass.
if [ "$status" -eq 0 ] && grep -qE '^test result: ok\. 0 passed' "$LOG"; then
  echo "" >&2
  echo "error: 0 tests ran — every test was filtered out, so this proves nothing." >&2
  echo "       Check the filter argument (and note this script already supplies the '--')." >&2
  exit 1
fi

# Third member of the family, and a slightly different failure from the two above: not a run that executed
# nothing, but a run whose green cannot be pinned to a JDK. `jdk_or_skip` prints one `JDK in use:` line per
# run; if it is missing from a passing log then either the harness stopped saying or nothing reached it,
# and in both cases the result is unattributable (TEST-18, #52). Cheap to keep honest, and the whole point
# of the change is that this line exists.
if [ "$status" -eq 0 ] && ! grep -q '^JDK in use:' "$LOG"; then
  echo "" >&2
  echo "error: the run never said which JDK it used, so a green result cannot be attributed to a" >&2
  echo "       version. Expected a 'JDK in use:' line from the harness — see Jdk::banner." >&2
  exit 1
fi

# The ranking (TEST-26, #103). Printed for a failing run too: "which tests are slow" is not a question that
# only matters when everything passed, and a test that failed on a timeout is exactly the one you want to
# see ranked. Never allowed to change the exit status — `|| true` because a broken stopwatch must not turn
# a green suite red, and the parser says so itself when it found nothing.
# `--refused` is passed on the fallback path because the retried run's log has no refusal in it — the tee
# truncated it — and without being told, the report would guess "a run that did not pass --report-time",
# which is the wrong diagnosis for the wrong reason.
if command -v python3 >/dev/null 2>&1; then
  label="$(sed -n 's/^JDK in use: \(javac [^ ]*\).*/\1/p' "$LOG" | head -1)"
  [ -n "$SHARD" ] && label="$label shard $SHARD"
  timing_args=(--label "$label")
  [ "$refused" -eq 1 ] && timing_args+=(--refused)
  scripts/test-timings.py "${timing_args[@]}" "$LOG" || true
else
  echo "" >&2
  echo "note: python3 not found, so no timing ranking. The durations are still in the log above." >&2
fi

# Say it again at the end. A hundred-odd tests of output scroll the banner far off the top, and "which JDK
# was that, actually?" is exactly the question nobody thought to ask for the hours #52 describes. Printed
# whether the run passed or failed — a red leg's version is worth as much as a green one's.
grep '^JDK in use:' "$LOG" || true

exit "$status"
