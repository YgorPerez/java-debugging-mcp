#!/usr/bin/env bash
# Does the public Rust API of this workspace still match the version number attached to it?
#
#     scripts/semver-check.sh                 # against the latest v* tag that is not HEAD
#     scripts/semver-check.sh v0.7.0          # against a tag you name
#     scripts/semver-check.sh --report-only   # report a break, do not fail on it
#
# ## Why a script and not `cargo semver-checks` in the workflow
#
# Because the tool's DEFAULT baseline is wrong here, and a git tag is right — for two different reasons
# that arrived in that order.
#
# ORIGINALLY the default was wrong because it answered from the wrong package. `cargo semver-checks` with
# no baseline compares against the crate's latest release on crates.io, and `jdwp-client` there is
# <https://github.com/bonk-dev/jdwp-client> — an unrelated project that registered the name in September
# 2025. Ours was unpublished, so the default run compared our crate against a stranger's and reported "no
# semver update required". That is why rust-doctor's own semver pass stays uninstalled
# (`.github/workflows/rust-doctor.yml` says so at the point of the decision): a pass that answers from the
# wrong package is worse than one that is skipped.
#
# THAT PARTICULAR TRAP IS GONE (REL-5, ADR-0043). The library is published as `java-debugging-jdwp-client`
# — the collision above is now the reason for its NAME rather than the reason for this file — so a default
# run would at last find our own package. Nothing here changes anyway, because the tag baseline was never
# only a workaround:
#
#   - A registry baseline can only compare against a RELEASE. Between releases, which is where this check
#     is read, the newest published version is the previous release; that is the same comparison
#     `--baseline-rev` makes, reached less directly and only once the publish has actually landed.
#   - It would make a LINT depend on the network and on a third party's uptime. `--baseline-rev` reads an
#     object out of this repository.
#   - It answers nothing on a commit whose version is not yet published, which is every commit this runs on.
#
# So `--baseline-rev` remains the flag this file exists for; what changed is that it is now a preference
# with reasons rather than the only option that worked.
#
# ## What it covers, and what it does not
#
# **Covers:** the public Rust API of both lib targets in the workspace — `jdwp-client` and, since CLEAN-3
# (#186), `jdwp-mcp`. This ran `--workspace` all along, so the second package arrived in the report the day
# it grew a `[lib]`; the sentence that used to stand here said `jdwp-mcp` was a `[[bin]]` contributing
# nothing, and it stopped being true in the commit that added the library.
#
# **`jdwp-mcp`'s library is not a supported API** and its findings should be read that way. Every item it
# exports is `#[doc(hidden)]` and exists so this repository's own tests can reach the request→reply path
# in-process; `mcp-server/src/lib.rs` says so in its crate docs. `#[doc(hidden)]` does NOT hide an item from
# cargo-semver-checks, so churn there will show up here — that is expected, it is reported rather than
# failed between releases for the reason below, and the documented stance is the mitigation rather than the
# attribute.
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

# The baseline has no LIBRARY for a package that has one now — the sibling of the rename below, and it has
# to be tested BEFORE the compile-error branch because it arrives wrapped in the same
# `failed to build rustdoc`.
#
# `--workspace` asks for every member and builds rustdoc for both sides. A member that was `[[bin]]`-only
# at the baseline has no rustdoc to build there, which cargo-semver-checks reports as
# `no library targets found in package X` and then escalates to `failed to build rustdoc for crate X`. Read
# through the branch below, that says "fix the build" about a tree that builds perfectly well.
#
# It is neither a break nor a build failure: there is no previous version of that API because there was no
# API, so no version bump resolves it and failing would be a red that stays red until the next release —
# the permanent-red defect this file's header is written against. Self-clearing for the reason the rename
# is: the next tag carries the `[lib]` and becomes a usable baseline.
#
# **Found the hard way.** CLEAN-3 (#186) gave `jdwp-mcp` a library and updated this file's "what it covers"
# section to say the package had arrived in the report — which was true of the working tree and not of any
# baseline that predates it. Nothing caught it for seven commits, because nothing had been pushed; the
# first push after it went red here, on a workspace whose every other check was green.
#
# The working tree is asked whether it really has the library, so that the opposite case — someone REMOVING
# a `[lib]`, where the baseline is right and the tree is not — still falls through to the failure below.
if grep -q "no library targets found in package" "$OUT"; then
  MISSING="$(sed -n 's/.*no library targets found in package `\([^`]*\)`.*/\1/p' "$OUT" | sort -u)"
  HAS_LIB_NOW="$(
    cargo metadata --no-deps --format-version 1 2>/dev/null |
      python3 -c '
import json, sys
meta = json.load(sys.stdin)
print("\n".join(p["name"] for p in meta["packages"] if any(t["kind"] == ["lib"] for t in p["targets"])))
' 2>/dev/null || true
  )"
  gained=""
  for pkg in $MISSING; do
    if printf '%s\n' "$HAS_LIB_NOW" | grep -qx "$pkg"; then gained="$gained $pkg"; fi
  done
  if [ -n "$gained" ]; then
    echo "**This did not check anything for$gained: the baseline has no library there.** \`$BASELINE\`"
    echo "predates the \`[lib]\`, so there is no previous version of that API to compare against — which is"
    echo "not a compatibility finding, and is not something a bigger version bump would resolve."
    echo
    echo "Expected exactly once per package that gains a library (CLEAN-3, #186 gave \`jdwp-mcp\` one). The"
    echo "next tag carries it and becomes a usable baseline, so this clears itself. **Every other package"
    echo "in the report above was compared normally** — read their result, not this paragraph, for whether"
    echo "the API moved."
    exit 0
  fi
fi

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

# The other way this checks nothing while exiting non-zero: the baseline has no package under the name the
# working tree uses, so there is nothing to compare against. That is a RENAME, not a break — and it is
# reported as `error: failed to retrieve local crate data from git revision`, which fell straight through to
# the "public API broke" branch below and produced a confident finding about a comparison that never ran.
# Found on REL-5's own rename (ADR-0043), which is the first time this repo has renamed a published crate.
#
# It exits 0, deliberately, including on the release path. A rename leaves nothing for any version number to
# permit, so failing would block the release with a red that NO bump can clear — the permanent-red defect
# this script's own header is written against. The message is the mitigation: it says what did not happen,
# and it names the one state in which this is expected, so a rename nobody intended does not read as routine.
#
# This is self-clearing. Once a tag exists carrying the new name, that tag becomes the baseline and the
# check resumes on its own; if this is still firing two releases later, the name is drifting every release and
# that is the actual bug.
if grep -qE "failed to retrieve local crate data from git revision|no crate named .* in baseline" "$OUT"; then
  echo "**This did not check anything: the baseline has no crate under this name.** \`$BASELINE\` predates a"
  echo "package rename, so there is no previous version of this API to compare against — which is not a"
  echo "compatibility finding, and is not something a bigger version bump would resolve."
  echo
  echo "Expected exactly once, on the first release after a rename (REL-5 renamed \`jdwp-client\` to"
  echo "\`java-debugging-jdwp-client\`; see ADR-0043). The next tag carries the new name and becomes a usable"
  echo "baseline, so this clears itself. **If you did not rename anything, something else did** — check"
  echo "\`[package] name\` in both members before going further."
  exit 0
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
