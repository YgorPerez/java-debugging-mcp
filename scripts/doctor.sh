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

exec npx -y "rust-doctor@${RUST_DOCTOR_VERSION}" "$@"
