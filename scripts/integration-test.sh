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
#
# Arguments go straight to libtest — this script already supplies the `--`, so do NOT pass another one.
# `… -- --test-threads=1` makes libtest read the bare `--` as a test-name FILTER, which matches nothing:
# "0 passed; 47 filtered out", exit 0, and a run that looks fine having executed nothing. The usage line
# above said exactly that until it was caught doing it.
#
# The `--test mcp_integration` scope keeps the output to these tests and skips rebuilding the other
# harnesses; `cargo test -- --ignored` also works if you want everything.
set -euo pipefail

cd "$(dirname "$0")/.."

LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

# pipefail is set, so ${PIPESTATUS[0]} is read before anything can overwrite it.
set +e
cargo test --test mcp_integration -- --ignored --nocapture "$@" | tee "$LOG"
status=${PIPESTATUS[0]}
set -e

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

# Say it again at the end. Sixty-five tests of output scroll the banner far off the top, and "which JDK
# was that, actually?" is exactly the question nobody thought to ask for the hours #52 describes. Printed
# whether the run passed or failed — a red leg's version is worth as much as a green one's.
grep '^JDK in use:' "$LOG" || true

exit "$status"
