#!/usr/bin/env bash
# Does the public Rust API of this workspace still match the version number attached to it?
#
#     scripts/semver-check.sh                 # against the latest v* tag that is not HEAD
#     scripts/semver-check.sh v0.7.0          # against a tag you name
#     scripts/semver-check.sh --report-only   # report a break, do not fail on it
#
# ## Why a script and not `cargo semver-checks` in the workflow
#
# Because the tool's DEFAULT baseline is wrong here in a way that produces a confident green answer.
# `cargo semver-checks` with no baseline compares against the crate's latest release on crates.io, and
# `jdwp-client` there is <https://github.com/bonk-dev/jdwp-client> — an unrelated project that registered
# the name in September 2025. Ours is unpublished (distribution is this repo's release binaries), so the
# default run compares our crate against a stranger's and reports "no semver update required". That is why
# rust-doctor's own semver pass stays uninstalled (`.github/workflows/rust-doctor.yml` says so at the
# point of the decision): a pass that answers from the wrong package is worse than one that is skipped.
#
# A git tag is the baseline that means something for an unpublished crate, and `--baseline-rev` is how you
# ask for it. That flag is the whole reason this file exists; nothing else here is interesting.
#
# ## What it covers, and what it does not
#
# **Covers:** `jdwp-client`'s public API. That is the only lib target in the workspace — `jdwp-mcp` is a
# `[[bin]]`, and cargo-semver-checks only reads libraries, so it contributes nothing to this check.
#
# **Does not cover the thing callers actually depend on**, which is the MCP tool surface: tool names,
# argument names, reply shapes. Nothing in Rust's type system knows that `debug.set_line_stop` renamed an
# argument, and `docs/toolkit-contract.md` exists because five of the six ways that breaks a consumer are
# silent. This check is not a substitute for the release notes; it is a second, narrower signal that keeps
# the *version number* honest, which matters because the downstream toolkit pins one.
#
# ## Why a break can be REPORTED rather than failed (`--report-only`, CI-2, #122)
#
# Between releases the working version equals the baseline tag, so cargo-semver-checks assumes the smallest
# bump and any break violates it. That is correct and it is also the NORMAL state of `main` for the whole
# development cycle: measured across two cycles, this job was the only failing one in every run between
# releases, and the only way to clear it was to cut the release — the one thing you are not doing yet.
#
# A red that is routine is the mirror of the defect the rest of this file is built against. `CLAUDE.md`
# already names the cost, in the section explaining why the AI code-review workflow was REMOVED rather than
# repaired: "a permanent red that tested nothing costs more than that: it teaches you to ignore red on PRs."
# It arrives on `pull_request` too, which is where that habit is cheapest to learn.
#
# So off a release ref the finding is printed in full — the findings, the versions, and the bump that would
# permit it — and the run is allowed to conclude green. On the path `release.yml` calls, nothing changes: the
# exit status is the tag's gate, which is what this check was always for.
#
# THE REJECTED ALTERNATIVE was to compute the bump the findings require and pass whenever a plausible NEXT
# version would permit them. Richer on `main`, and rejected because it encodes an assumption nothing else in
# this repo makes: no one has decided that the next release is a minor bump, and a green tick resting on that
# guess would mean more than it should — which is the exact failure `--findings`, the SARIF filter and the
# "0 checks ran" message below all exist to prevent. Naming the permitting bump in the REPORT costs nothing
# and claims nothing, so that half of the idea is kept.
#
# ## The vacuous pass
#
# When the working version is a major step above the baseline — and for 0.x, a minor bump IS a major step —
# every check skips, because a major bump is permitted to break anything. So on a release commit this
# verifies NOTHING while exiting 0, which is exactly the shape of green run this repo keeps getting caught
# by. It says so out loud instead: "0 checks ran" is printed as a finding about the run, not omitted
# because it is not a finding about the code.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v cargo-semver-checks >/dev/null 2>&1; then
  echo "error: cargo-semver-checks is not installed." >&2
  echo "       cargo install cargo-semver-checks --locked   (or use taiki-e/install-action in CI)" >&2
  exit 1
fi

# `--report-only` in any argument position, so `semver-check.sh v0.7.0 --report-only` reads naturally.
REPORT_ONLY=0
ARGS=()
for arg in "$@"; do
  case "$arg" in
    --report-only) REPORT_ONLY=1 ;;
    *) ARGS+=("$arg") ;;
  esac
done

BASELINE="${ARGS[0]:-}"
if [ -z "$BASELINE" ]; then
  # The latest v* tag that is NOT on HEAD. Excluding HEAD's own tag is what makes this work during a
  # release: `release.sh` tags the bump commit, the gate then runs on that tag, and a baseline of "the tag
  # I am" would compare the tree against itself and call it verified.
  CURRENT_TAG="$(git tag --points-at HEAD --list 'v*' | head -1)"
  if [ -n "$CURRENT_TAG" ]; then
    BASELINE="$(git describe --tags --abbrev=0 --match 'v*' --exclude "$CURRENT_TAG" 2>/dev/null || true)"
  else
    BASELINE="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"
  fi
fi

if [ -z "$BASELINE" ]; then
  echo "## semver: nothing to compare against"
  echo
  echo "No \`v*\` tag before HEAD, so there is no baseline and **nothing was verified**. In CI this usually"
  echo "means the checkout was shallow or tags were not fetched (\`fetch-depth: 0\`), not that the"
  echo "repository has no releases — so treat a green tick here as absent evidence."
  exit 0
fi

echo "## semver: \`$BASELINE\` → working tree"
echo

OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT
status=0
cargo semver-checks --workspace --baseline-rev "$BASELINE" >"$OUT" 2>&1 || status=$?

# `Checked [ 0.0s] 253 checks: 0 pass, 253 skip` — the number that matters is how many actually RAN.
ran="$(sed -n 's/.*\([0-9][0-9]*\) checks: \([0-9][0-9]*\) pass.*/\2/p' "$OUT" | paste -sd+ - | bc 2>/dev/null || echo 0)"
ran="${ran:-0}"

echo '```'
grep -E "Checking|Summary|checks:|^ *--- failure|^error" "$OUT" || cat "$OUT"
echo '```'
echo

# A crate that does not build is not a crate with a broken API, and saying the second when the first is
# true sends the reader looking for a compatibility problem that isn't there. cargo-semver-checks builds
# rustdoc for both sides, so either side failing to compile arrives through the same non-zero exit.
if grep -qE "failed to build rustdoc|could not document|running cargo-doc on crate .* failed" "$OUT"; then
  echo "**This did not check anything: rustdoc failed to build.** That is a compile error, not a"
  echo "compatibility finding — fix the build and run it again. If the failure is in the BASELINE"
  echo "(\`$BASELINE\`) rather than the working tree, the tag itself does not build, which is worth knowing"
  echo "separately."
  exit "${status:-1}"
fi

if [ "$status" -ne 0 ]; then
  # The version the working tree declares, and the smallest release that would permit these findings. For
  # 0.x cargo-semver-checks treats the MINOR as the major component, so 0.14.1 -> 0.15.0 is the permitting
  # bump and 0.14.2 is not. Stated rather than assumed, because verifying it by hand — bump Cargo.toml, run
  # the check, watch every check skip, revert — is what this cost every cycle.
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
  PERMITS="$(echo "$VERSION" | awk -F. '{ if ($1 == "0") printf "0.%d.0", $2 + 1; else printf "%d.0.0", $1 + 1 }')"

  echo "**The public API broke without a version that allows it.** Either the change is a mistake, or the"
  echo "next release is a bigger bump than planned — and \`docs/toolkit-contract.md\` wants it in the release"
  echo "notes either way. Full output above."
  echo
  echo "The working tree declares \`$VERSION\` and the baseline is \`$BASELINE\`, so the smallest bump that"
  echo "permits these findings is **\`$PERMITS\`** (for 0.x the minor is the major component, so a patch"
  echo "release would not)."

  if [ "$REPORT_ONLY" -eq 1 ]; then
    echo
    echo "**Reported, not failed** (\`--report-only\`). Between releases the working version equals the"
    echo "baseline tag, so this is the normal state of \`main\` and the only way to clear it is to cut the"
    echo "release. Failing here every cycle teaches you to ignore red on PRs, which costs more than it"
    echo "catches — see the header of this script for the argument and the rejected alternative. The gate is"
    echo "the release path, where this same state still exits non-zero and blocks the tag."
    exit 0
  fi
  exit "$status"
fi

if [ "$ran" -eq 0 ]; then
  echo "**0 checks ran, so this verified nothing.** Every check skipped, which is what happens when the"
  echo "working version is already a major step above \`$BASELINE\` (for 0.x, a minor bump is a major step):"
  echo "a bump that permits breaking changes leaves nothing to violate. Expected on a release commit."
  echo "Do NOT read this as \"the API is compatible\" — it is \"the question does not apply\"."
  exit 0
fi

echo "**$ran check(s) ran and passed** against \`$BASELINE\`: the public API is compatible with the version"
echo "currently declared, so a patch release is defensible on these grounds. Grounds this cannot speak to:"
echo "the MCP tool surface — see \`docs/toolkit-contract.md\`."
