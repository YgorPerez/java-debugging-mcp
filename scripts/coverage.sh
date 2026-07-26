#!/usr/bin/env bash
#
# Coverage over BOTH layers — unit tests and the MCP-level integration tests. The integration tests are
# what exercise most of the interesting code (the event pump, the resume paths, expression resolution), and
# they are `#[ignore]`d and need a JDK, so a run that omits them measures almost nothing while looking
# perfectly healthy. That is the same trap `tests.yml` guards with its no-SKIP check, so this guards it too:
# a SKIP line is a hard failure here, not a footnote.
#
# Why coverage exists in this repo at all: every safety bug found across five review rounds was in an
# untested *state* (disconnect while suspended, the watchdog after a manual pause, a resume at suspend
# depth 2). Those were found by reading code and guessing — a method that also produced three tests which
# passed against the very bug they were written for. Coverage cannot name a missing state, but it names the
# branch nobody reaches, which is where they hide.
#
# Usage:
#   scripts/coverage.sh                 # summary table to stdout + lcov at target/coverage/lcov.info
#   scripts/coverage.sh --html          # also write an HTML report to target/llvm-cov/html
#   scripts/coverage.sh --uncovered     # list uncovered regions, most-uncovered file first
#
# There is deliberately NO percentage gate. The value is the branch list, not a number to defend.
#
# Requires: cargo-llvm-cov + the llvm-tools component, and a JDK (javac) for the probes.
#   rustup component add llvm-tools-preview && cargo install cargo-llvm-cov
set -euo pipefail

cd "$(dirname "$0")/.."

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "error: cargo-llvm-cov not installed. Run:" >&2
  echo "  rustup component add llvm-tools-preview && cargo install cargo-llvm-cov" >&2
  exit 1
fi

# `-C instrument-coverage` needs the profiler runtime, which rustup ships as part of the target's std —
# and NOT for every target. On `x86_64-pc-windows-gnu` it is absent, so the run dies ~90 seconds in with
# `error[E0463]: can't find crate for profiler_builtins` buried under a full rustc command line, which
# names the missing crate but not the cause or the fix. Checked up front instead: the tool is not
# installable on this host, and finding that out before a two-minute build is the whole point.
HOST_TARGET="$(rustc -vV | awk '/^host: /{print $2}')"
SYSROOT="$(rustc --print sysroot)"
if ! ls "$SYSROOT/lib/rustlib/$HOST_TARGET/lib/"libprofiler_builtins-*.rlib >/dev/null 2>&1; then
  echo "error: this toolchain has no profiler runtime, so coverage cannot be instrumented here." >&2
  echo "       host target: $HOST_TARGET" >&2
  echo "       (looked for libprofiler_builtins-*.rlib in \$SYSROOT/lib/rustlib/$HOST_TARGET/lib/)" >&2
  echo "" >&2
  echo "  Known case: x86_64-pc-windows-gnu ships no profiler_builtins. Either" >&2
  echo "    - run this on Linux (what CI uses), or" >&2
  echo "    - switch to the msvc toolchain, which does have it:" >&2
  echo "        rustup toolchain install stable-x86_64-pc-windows-msvc" >&2
  echo "      note msvc also needs the Visual Studio 'C++ build tools' workload for link.exe;" >&2
  echo "      without it the toolchain has the profiler runtime but cannot link at all." >&2
  exit 1
fi

MODE="${1:-summary}"
LCOV="target/coverage/lcov.info"
LOG="target/coverage/run.log"
mkdir -p target/coverage

# `--include-ignored` picks up the integration tests alongside the unit tests, so one profile covers both
# layers. Scoped to the mcp_integration target for the ignored set, mirroring scripts/integration-test.sh:
# a bare `--ignored` also un-ignores jdwp-client's illustrative ```ignore doctests, which were never meant
# to compile (see DOC-2).
echo "==> running unit + integration tests under instrumentation (this compiles from scratch; ~1-2 min)"
cargo llvm-cov --no-report --workspace --tests -- --include-ignored 2>&1 | tee "$LOG" | tail -20

# A skipped integration test means no JDK — the run would report cheerfully low coverage of code that was
# never executed. Fail instead.
if grep -q 'SKIP .*no JDK found' "$LOG"; then
  echo "::error:: integration tests SKIPPED (no JDK) — coverage would be measured with the interesting" >&2
  echo "          half of the suite not running. Install a JDK (javac) and re-run." >&2
  grep 'SKIP .*no JDK found' "$LOG" >&2
  exit 1
fi

# lcov for rust-doctor, which looks for exactly this path.
cargo llvm-cov report --lcov --output-path "$LCOV" >/dev/null
echo "==> lcov written to $LCOV (rust-doctor reads this path)"

case "$MODE" in
  --html)
    cargo llvm-cov report --html >/dev/null
    echo "==> HTML report at target/llvm-cov/html/index.html"
    ;;
  --uncovered)
    # Region coverage ascending: the files with the most unexercised branches come first, which is the
    # list worth actually reading.
    cargo llvm-cov report --summary-only
    ;;
  *)
    cargo llvm-cov report --summary-only
    ;;
esac
