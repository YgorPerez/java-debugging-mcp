#!/usr/bin/env bash
#
# Local rust-doctor health check — the same tool the CI runs (arthjean/rust-doctor), pinned to the
# latest stable release. Uses `npx` to fetch a prebuilt native binary, so no Rust build of the tool
# is needed (it still shells out to your local `cargo clippy`). Requires Node/npx.
#
# Usage:
#   scripts/doctor.sh                  # 0–100 score card for the whole workspace
#   scripts/doctor.sh --verbose        # per-finding file:line detail
#   scripts/doctor.sh --plan           # prioritized remediation plan
#   scripts/doctor.sh --diff main      # only files changed vs the `main` branch
#   scripts/doctor.sh --json           # machine-readable JSON to stdout
#   scripts/doctor.sh --fail-on error  # exit 3 if any errors (for pre-push gating)
#
# Any rust-doctor flag passes straight through. Override the version with RUST_DOCTOR_VERSION.
set -euo pipefail

RUST_DOCTOR_VERSION="${RUST_DOCTOR_VERSION:-0.2.0}"

if ! command -v npx >/dev/null 2>&1; then
  echo "error: npx (Node.js) not found — install Node, or 'cargo install rust-doctor'." >&2
  exit 1
fi

# Run from the repo root regardless of the caller's cwd.
cd "$(dirname "$0")/.."

# The gate runs on a PINNED toolchain, and clippy's lint set changes between releases — so a local run on
# a different rustc reports a different answer, and the direction is the dangerous one: an OLDER toolchain
# simply does not have the newer lints, so it prints "0 warnings" for code the gate will fail on.
#
# That is not hypothetical. It cost a red `main`: two `Duration::from_millis(1000)` in test code passed a
# local 1.94 run at 100/100 and failed CI on `clippy::duration_suboptimal_units`, a lint 1.97 added. The
# same shape as this repo's other green-runs-of-nothing (SIGKILL'd coverage counters, an undetectable JDK,
# a filter matching no tests): a check that reported success without having looked.
#
# Read from the workflow rather than duplicated here, so the two cannot drift apart. A mismatch warns
# rather than fails: running on whatever you have is still worth doing, as long as you know what it means.
PINNED_TOOLCHAIN="$(sed -n 's/.*toolchain: *"\([0-9][0-9.]*\)".*/\1/p' .github/workflows/rust-doctor.yml | head -1)"
ACTIVE_TOOLCHAIN="$(rustc -vV 2>/dev/null | sed -n 's/^release: *//p')"
if [ -n "$PINNED_TOOLCHAIN" ] && [ -n "$ACTIVE_TOOLCHAIN" ] && [ "$PINNED_TOOLCHAIN" != "$ACTIVE_TOOLCHAIN" ]; then
  cat >&2 <<EOF
warning: this run uses rustc ${ACTIVE_TOOLCHAIN}, but the CI gate is pinned to ${PINNED_TOOLCHAIN}.
         Clippy's lints differ between releases, so THIS RESULT IS NOT THE GATE'S — a clean run here can
         still fail CI (and did: see the note in this script). To check what CI will check:
           rustup toolchain install ${PINNED_TOOLCHAIN} --component clippy
           RUSTUP_TOOLCHAIN=${PINNED_TOOLCHAIN} scripts/doctor.sh $*
EOF
fi

exec npx -y "rust-doctor@${RUST_DOCTOR_VERSION}" "$@"
