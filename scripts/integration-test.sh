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

exit "$status"
