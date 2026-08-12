#!/usr/bin/env bash
#
# Local rust-doctor health check — the same tool the CI runs (arthjean/rust-doctor), pinned to the
# latest stable release. Fetches a prebuilt native binary, so no Rust build of the tool is needed (it
# still shells out to your local `cargo clippy`). Requires curl and tar.
#
# **Fetched from the upstream GitHub release, not from npm** (BUILD-1, #66). This used to be
# `npx -y rust-doctor@0.2.0`, and on 2026-07-29T10:49Z the package was *unpublished* from the npm
# registry — not yanked to a different version, removed. `npx` then failed with `ETARGET` for every value
# of RUST_DOCTOR_VERSION, so ADR-0007's gate could not run at all, locally or in CI, while nothing in this
# repo had changed. The tool itself was fine; only its distribution disappeared.
#
# GitHub release assets are the more durable source: same v0.2.0, same binary, and deleting a published
# release asset is a deliberate act rather than a one-command `npm unpublish`. The binary is cached under
# the user's cache dir keyed by version, so a re-run costs nothing and an offline machine keeps working
# once it has fetched.
#
# What this does NOT fix: the asset is unverified. Upstream publishes no SHA256SUMS with the release, so
# unlike the toolkit's own installer there is no manifest to check the download against, and pinning by
# version is all that is available. Worth stating rather than implying a chain of trust that is not there.
#
# Usage:
#   scripts/doctor.sh                  # 0–100 score card for the whole workspace
#   scripts/doctor.sh --findings       # the findings the gate counts, and whether it would pass (exit 3)
#   scripts/doctor.sh --verbose        # per-finding file:line detail
#   scripts/doctor.sh --plan           # prioritized remediation plan
#   scripts/doctor.sh --diff main      # only files changed vs the `main` branch
#   scripts/doctor.sh --json           # machine-readable JSON to stdout
#   scripts/doctor.sh --fail-on error  # exit 3 if any errors (for pre-push gating)
#
# `--findings` composes with the rest: `scripts/doctor.sh --findings --diff main` lists only what you
# changed. Any rust-doctor flag passes straight through. Override the version with RUST_DOCTOR_VERSION.
set -euo pipefail

RUST_DOCTOR_VERSION="${RUST_DOCTOR_VERSION:-0.2.0}"

for tool in curl tar; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool not found — needed to fetch rust-doctor's prebuilt binary." >&2
    exit 1
  fi
done

# The release publishes one asset per target triple; pick this machine's.
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) RD_TARGET="x86_64-unknown-linux-gnu"; RD_EXT="tar.gz" ;;
  Linux-aarch64 | Linux-arm64) RD_TARGET="aarch64-unknown-linux-gnu"; RD_EXT="tar.gz" ;;
  Darwin-arm64) RD_TARGET="aarch64-apple-darwin"; RD_EXT="tar.gz" ;;
  Darwin-x86_64) RD_TARGET="x86_64-apple-darwin"; RD_EXT="tar.gz" ;;
  *)
    echo "error: no rust-doctor release asset for $(uname -s)-$(uname -m)." >&2
    echo "       Set RUST_DOCTOR_BIN to a rust-doctor binary you built yourself." >&2
    exit 1
    ;;
esac

# `RUST_DOCTOR_BIN` short-circuits the fetch entirely — for an unsupported platform, an air-gapped
# machine, or bisecting against a locally built tool.
RD_CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/rust-doctor/${RUST_DOCTOR_VERSION}"
RUST_DOCTOR_BIN="${RUST_DOCTOR_BIN:-$RD_CACHE/rust-doctor}"

if [ ! -x "$RUST_DOCTOR_BIN" ]; then
  RD_URL="https://github.com/arthjean/rust-doctor/releases/download/v${RUST_DOCTOR_VERSION}/rust-doctor-${RD_TARGET}.${RD_EXT}"
  echo "fetching rust-doctor ${RUST_DOCTOR_VERSION} (${RD_TARGET})…" >&2
  mkdir -p "$RD_CACHE"
  # Into a temp dir first, so an interrupted download cannot leave a half-extracted binary in the cache
  # that later runs would treat as good.
  RD_TMP="$(mktemp -d)"
  trap 'rm -rf "$RD_TMP"' EXIT
  # Retried on the same flags as `.github/workflows/rust-doctor.yml`, because this script and that
  # workflow fetch identically on purpose and a retry only one of them has is a difference to rediscover.
  # Both flags earn their place against a different observed failure: plain `--retry` covers the `503`
  # from the release CDN (5xx is transient by curl's own list) but NOT the `(56) Connection died`, which
  # needs `--retry-all-errors`. Both took the CI gate down on 2026-08-12.
  #
  # The cost is that a withdrawn release — BUILD-1's actual failure — now fails in ~10 s rather than ~2 s,
  # measured locally against an absent tag, still with the `404` this message is written for.
  if ! curl -fsSL --retry 3 --retry-all-errors --retry-delay 2 --retry-max-time 30 \
    "$RD_URL" -o "$RD_TMP/rd.${RD_EXT}"; then
    echo "error: could not download $RD_URL" >&2
    echo "       If the release moved or was withdrawn, see BUILD-1 (#66) — the npm distribution was" >&2
    echo "       already lost this way. Set RUST_DOCTOR_BIN to a binary you have." >&2
    exit 1
  fi
  tar xzf "$RD_TMP/rd.${RD_EXT}" -C "$RD_TMP"
  RD_FOUND="$(find "$RD_TMP" -type f -name 'rust-doctor*' -perm -u+x | head -1)"
  if [ -z "$RD_FOUND" ]; then
    echo "error: the release asset contained no rust-doctor binary." >&2
    exit 1
  fi
  mv "$RD_FOUND" "$RUST_DOCTOR_BIN"
  chmod +x "$RUST_DOCTOR_BIN"
fi

# Run from the repo root regardless of the caller's cwd.
cd "$(dirname "$0")/.."

# One clippy.toml, at the root, and this is what makes rust-doctor read it (LINT-2, #28). rust-doctor
# writes its own temporary `<crate>/clippy.toml` into any workspace member that has none, and clippy
# stops at the first config it finds walking up from the crate — so the injected file shadows ours, and
# the `syn` duplication this repo has already accepted comes back as a warning per crate. Pointing
# clippy at a directory skips the walk. Without it a local run disagrees with the gate in the noisy
# direction, which is at least the direction you notice. See clippy.toml.
export CLIPPY_CONF_DIR="$PWD"

# The gate runs on a PINNED toolchain, and clippy's lint set changes between releases — so a local run on
# a different rustc reports a different answer, and the direction is the dangerous one: an OLDER toolchain
# simply does not have the newer lints, so it prints "0 warnings" for code the gate will fail on.
#
# That is not hypothetical. It cost a red `main`: two `Duration::from_millis(1000)` in test code passed a
# local 1.94 run at 100/100 and failed CI on `clippy::duration_suboptimal_units`, a lint 1.97 added. The
# same shape as this repo's other green-runs-of-nothing (SIGKILL'd coverage counters, an undetectable JDK,
# a filter matching no tests): a check that reported success without having looked.
#
# Read from rust-toolchain.toml rather than duplicated here, so the two cannot drift apart. A mismatch
# warns rather than fails: running on whatever you have is still worth doing, as long as you know what it
# means.
#
# THIS IS NOW A BACKSTOP RATHER THAN THE MECHANISM (LINT-5, #141). rustup honours rust-toolchain.toml on
# its own, so in a normal clone the active toolchain already IS the pin and this warning stays quiet. It
# is kept for the cases where the file cannot do its job: `RUSTUP_TOOLCHAIN` set in the environment (which
# outranks it), a `+toolchain` override, or a rustc that rustup does not manage. A quiet warning that has
# nothing left to warn about is not the same as one that was deleted — this one still fires on the
# override that reintroduces the exact failure it was written for.
PINNED_TOOLCHAIN="$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml | head -1)"
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

# The one persistent `✗` on `windows-gnu` is not a finding about this code, and reading it as one is the
# worst outcome available: an error line that is always there and never actionable teaches you to skip the
# error line. Doctor builds into its own `target/rust-doctor`, which cannot link there
# (`ld.exe: cannot find \symbols.o` — path mangling in that separate build dir), and a build that cannot
# link is a clippy pass that cannot run. So the run reports the custom AST rules ONLY and contributes zero
# clippy findings, while still printing a warning count as if it had looked. LINT-1 was verified locally at
# 0 warnings on exactly that and CI failed the new gate on three clippy findings Windows never went looking
# for (ADR-0007).
#
# Told apart by the host triple, not by matching the message — so this says the half that is worth knowing
# ("your clippy findings are missing") rather than the half that trains you to ignore the screen.
HOST_TRIPLE="$(rustc -vV 2>/dev/null | sed -n 's/^host: *//p')"
case "$HOST_TRIPLE" in
*windows-gnu*)
  cat >&2 <<EOF
warning: host is ${HOST_TRIPLE}, where doctor's isolated build cannot link — so its clippy pass CANNOT RUN.
         The ✗ below is that link failure, not a finding about this code. The counts below cover the custom
         AST rules only; the clippy half of this run is missing, not zero. Get it separately (ADR-0007):
           cargo clippy --workspace --all-targets -- -W clippy::pedantic -W clippy::nursery -W clippy::cargo
EOF
  ;;
esac

# --findings: the findings behind the summary box, in the shape CI already prints them in.
#
# The box hands you `⚠ 5 warning(s)` and no way to reach the five. What detail there is goes to STDERR,
# aggregated to one line per message with an occurrence count and no file:line at all — so
# `scripts/doctor.sh > out.txt` keeps the counts and throws the findings away, and grepping that file for
# `⚠`, for `warning`, for the rule name, for `threshold` misses every time. Here is what that cost: v0.2.0's
# tag build passed the version check, all four platform builds and the whole test suite, and then failed
# the lint gate on five `excessive-clone` findings that had every one been sitting in a local run
# beforehand. The count was watched going 1 → 5 and waved off, because `cargo clippy --all-targets` was
# clean and there was no cheap way to see WHAT the five were. They only became legible afterwards, in CI's
# step summary. This prints those same lines from the same structured output, before the push (#42).
#
# Parsed with `node` rather than `python3` (which is what the workflow uses). That choice was originally
# free: the script fetched the tool with `npx`, so Node was already a hard requirement. It is no longer —
# BUILD-1 (#66) replaced `npx` with a plain release download, needing only curl and tar — so `--findings`
# now checks for Node itself, below, instead of relying on a requirement that has since been removed.
# Left as Node rather than switched to python3 because that is a behaviour change to the parsing on a day
# the gate is already being repaired; the check makes the dependency honest either way.
#
# `--findings` is ours, so it is stripped before the rest is handed on; `--json` too, since this mode adds
# its own. Everything else passes through in order, which is what keeps `--diff main` and friends working.
FINDINGS=0
PASSTHRU=()
for arg in "$@"; do
  case "$arg" in
  --findings) FINDINGS=1 ;;
  --json) ;;
  *) PASSTHRU+=("$arg") ;;
  esac
done

if [ "$FINDINGS" -eq 1 ]; then
  SCAN="$(mktemp)"
  trap 'rm -f "$SCAN"' EXIT

  # Exit 3 is a tripped `--fail-on` gate, which still produced a complete scan; anything else means we have
  # no scan to read, and printing "0 findings" over the top of a crash is the failure shape this repo keeps
  # paying for.
  status=0
  "$RUST_DOCTOR_BIN" ${PASSTHRU[@]+"${PASSTHRU[@]}"} --json >"$SCAN" || status=$?
  if [ "$status" -ne 0 ] && [ "$status" -ne 3 ]; then
    echo "error: rust-doctor exited ${status} without completing a scan — no findings to report." >&2
    exit "$status"
  fi

  # WHICH optional tools the gate's environment installs, read from the workflow rather than asserted here,
  # for the same reason the toolchain pin is read from it: two copies of a fact drift.
  #
  # It reads the install step's `tool:` list, and it used to be a yes/no `grep -E 'install-deps|cargo
  # +install'` over the whole file. That was wrong in both directions and failed in the direction that
  # matters: the workflow gained a COMMENT containing the words "cargo install" and the answer flipped to
  # yes, which silenced the "ran here, but not in the gate" section below — the section whose entire job is
  # to stop a local verdict being read as CI's. Prose cannot be allowed to answer a question about
  # configuration. Yes/no was also too coarse to be true any more: CI installs cargo-deny, cargo-machete
  # and cargo-shear, and deliberately does not install cargo-geiger or cargo-semver-checks, so the honest
  # answer is a list. (cargo-shear appears in that list but matches no rust-doctor pass — it is a step of
  # its own, for the reason the workflow gives at it. Extra names here are harmless: the list is only ever
  # asked whether a pass that RAN has its tool in CI.)
  #
  # `head -1` IS THE FIX FOR A REAL BUG, and the bug is worth keeping described because the broken version
  # produced a correct answer by accident. `tr -d '[:space:]'` deletes newlines as well as spaces, so two
  # `tool:` lines collapsed into one string before `paste` could join them and the list came out as
  # `…,cargo-machetecargo-semver-checks` — one nonsense entry where there should have been two names. The
  # accident: the second `tool:` line belongs to the `semver` JOB, which installs cargo-semver-checks for
  # `scripts/semver-check.sh`. Gluing it to its neighbour kept it out of the list, and out is right —
  # rust-doctor's own semver pass is deliberately NOT in the gate (it would compare against the wrong
  # crate), so it must keep reporting as "ran here, but not in the gate". Repairing only the `tr` would
  # have added cargo-semver-checks to the list and silenced that line, which is the one outcome worse than
  # the bug. So: scope the question to the health job's install step, which is what "does the SCAN have
  # this tool" actually means, and strip blanks rather than all whitespace.
  #
  # If the health job ever gains a second install step, this reads only the first. Prefer keeping that
  # job's tools on the one `tool:` line above adding another step.
  CI_TOOLS="$(sed -n 's/^[[:space:]]*tool:[[:space:]]*//p' .github/workflows/rust-doctor.yml 2>/dev/null |
    head -1 | tr -d '[:blank:]')"

  verdict=0
  if ! command -v node >/dev/null 2>&1; then
    echo "error: --findings needs 'node' to parse the scan (see the note above; #66 removed the npx" >&2
    echo "       requirement that used to guarantee it). Install Node, or use 'scripts/doctor.sh --json'" >&2
    echo "       and read the findings from that." >&2
    exit 1
  fi
  SCAN_JSON="$SCAN" CI_TOOLS="$CI_TOOLS" node <<'JS' || verdict=$?
const fs = require("fs");
const scan = JSON.parse(fs.readFileSync(process.env.SCAN_JSON, "utf8"));

const diags = Array.isArray(scan.diagnostics) ? scan.diagnostics : [];
// What `--fail-on warning` counts, which is what the gate is. `info` is the rest of the score card.
const gated = diags.filter((d) => d.severity === "error" || d.severity === "warning");
const rank = (d) => (d.severity === "error" ? 0 : 1);
gated.sort(
  (a, b) =>
    rank(a) - rank(b) ||
    String(a.rule || "").localeCompare(String(b.rule || "")) ||
    String(a.file_path || "").localeCompare(String(b.file_path || "")) ||
    (a.line || 0) - (b.line || 0),
);

const score = `${scan.score}/100 "${scan.score_label}"`;
const out = [];
out.push(
  `## rust-doctor — ${gated.length} warning/error finding(s)` +
    `  [score ${score}, ${scan.source_file_count} files, ${Number(scan.elapsed).toFixed(1)}s]`,
);
out.push("");

// Same two lines the workflow's step summary prints, so a finding reads identically whether you found it
// here or in the build that failed on it. `help` is the third line, and is local-only: CI has the SARIF.
for (const d of gated) {
  const where = d.line ? `${d.file_path}:${d.line}` : `${d.file_path}`;
  out.push(`- **${d.severity}** \`${d.rule}\` — \`${where}\``);
  out.push(`  ${d.message}`);
  if (d.help) out.push(`  ↳ ${d.help}`);
}
if (gated.length) out.push("");

const tally = new Map();
for (const d of gated) tally.set(d.rule, (tally.get(d.rule) || 0) + 1);
const byRule = [...tally.entries()]
  .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
  .map(([rule, n]) => `${rule} ×${n}`)
  .join(", ");

// The two ways this verdict is narrower than CI's. Both get printed every time, because the reason this
// mode exists at all is a number that was read past what it knew.
const skipped = Array.isArray(scan.skipped_passes) ? scan.skipped_passes : [];
const ran = [...new Set((scan.pass_timings || []).map((p) => p.pass))];
// Per tool, not per run: a pass that ran here is only outside the gate if CI does not install the tool it
// needs. The pass label carries the tool name ("dependencies (cargo-machete)"), which is what makes this
// checkable rather than a list somebody keeps in sync by hand.
const ciTools = (process.env.CI_TOOLS || "").split(",").filter(Boolean);
const localOnly = ran.filter((p) => {
  const tool = (p.match(/\((cargo-[a-z-]+)\)/) || [])[1];
  return tool && !ciTools.includes(tool) && !skipped.some((s) => s.startsWith(p));
});

if (gated.length) {
  out.push(
    `gate: WOULD FAIL on this scan — ${scan.warning_count} warning(s) and ${scan.error_count} error(s) ` +
      `against CI's \`--fail-on warning\`.`,
  );
  out.push(`      by rule: ${byRule}`);
} else {
  out.push(`gate: would pass on this scan — no warning or error findings in the passes that ran.`);
}
out.push(`      The score cannot answer that question: it read ${score} on this very scan.`);
if (skipped.length || localOnly.length) {
  out.push(`      "On this scan" is doing work in that sentence — see below for what it is not CI.`);
}

// A pass over zero files passes. `--diff` against a base you have not diverged from is the easy way to
// get one, and it prints the same "would pass" as a real clean run — the green-run-of-nothing this repo
// has now hit five separate ways. Say the file count out loud rather than let it be the quiet part.
if (!scan.source_file_count) {
  out.push("");
  out.push(`nothing was scanned: 0 source files. That verdict is not about your code.`);
  out.push(`      With --diff, it means nothing you changed is a file the scanner reads. Without it, the`);
  out.push(`      scan did not find the workspace at all.`);
}

if (skipped.length) {
  out.push("");
  out.push(`not looked at here — ${skipped.length} pass(es) skipped for a missing tool:`);
  for (const s of skipped) out.push(`      ${s}`);
  out.push(`      A pass that did not run reports nothing, which reads exactly like one that found nothing.`);
}

if (localOnly.length) {
  out.push("");
  out.push(`ran here, but not in the gate — ${localOnly.length} pass(es) you have the tool for:`);
  for (const p of localOnly) out.push(`      ${p}`);
  out.push(`      .github/workflows/rust-doctor.yml installs none of these, so the gate never sees what`);
  out.push(`      they find. Findings above from one of them will not fail CI — and will not be fixed by`);
  out.push(`      CI passing, either.`);
}

process.stdout.write(out.join("\n") + "\n");
// Same exit code rust-doctor's own gate uses, so this is usable as a pre-push check.
process.exit(gated.length ? 3 : 0);
JS

  # cargo-shear gates in CI beside this scan, and rust-doctor's `dependencies` pass cannot reach what
  # it checks: that pass runs cargo-machete, which compares what a PACKAGE declares against what that
  # package's sources use — and an entry in the root `[workspace.dependencies]` is neither. Both members
  # here take their dependencies through `<name>.workspace = true`, so a workspace entry whose last user
  # goes away is dead weight machete reports as "Good job!". Measured, not assumed; the workflow carries
  # the table.
  #
  # It runs HERE rather than only in CI so that `--findings` keeps meaning what it says. This script's
  # whole claim is that a clean local run is a green gate; a check that gates in CI and not here would
  # quietly retire that claim, which is the defect the "ran here, but not in the gate" section above
  # exists to prevent — in the other direction.
  if command -v cargo-shear >/dev/null 2>&1; then
    shear_status=0
    shear_out="$(cargo shear 2>&1)" || shear_status=$?
    if [ "$shear_status" -ne 0 ]; then
      printf '\n%s\n' "unused dependencies (cargo-shear): WOULD FAIL — this gates in CI."
      printf '%s\n' "$shear_out" | sed 's/^/      /'
      verdict=3
    else
      printf '\n%s\n' "unused dependencies (cargo-shear): would pass."
    fi
  else
    # A pass that did not run reports nothing, which reads exactly like one that found nothing. That
    # sentence is already in the skipped-pass section above; this is the same rule applied to our own step.
    printf '\n%s\n' "not looked at here — cargo-shear is not installed, and it GATES in CI."
    printf '%s\n' "      A clean run above is therefore not a green gate. Install it with"
    printf '%s\n' "      \`cargo install cargo-shear --locked\` (or \`taiki-e/install-action\`, as CI does)."
  fi

  # rustdoc gates in CI beside this scan (DOC-13, #143), for the same reason cargo-shear does: rust-doctor
  # has no rustdoc pass and takes no option to add one, so the check lives next to it. And it runs HERE for
  # the reason the block above runs here — a check that gates in CI and not locally retires this script's
  # whole claim that a clean run is a green gate.
  #
  # Unlike cargo-shear this needs no separate binary: `cargo doc` ships with every toolchain. So there is
  # no "not installed" branch to write and no way for this one to quietly not run, which is the only
  # reason this block is shorter than that one rather than sloppier.
  #
  # Only `error`/`warning` lines are echoed. rustdoc prints the offending source line and several help
  # notes per finding, and 85 findings' worth of that (which is what this found on the day it was added)
  # buries the list it is meant to hand you. The full output is one `cargo doc` away.
  rustdoc_status=0
  rustdoc_out="$(RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items 2>&1)" ||
    rustdoc_status=$?
  if [ "$rustdoc_status" -ne 0 ]; then
    printf '\n%s\n' "documentation (rustdoc): WOULD FAIL — this gates in CI."
    printf '%s\n' "$rustdoc_out" | grep -E '^(error|warning)' | sed 's/^/      /'
    verdict=3
  else
    printf '\n%s\n' "documentation (rustdoc): would pass."
  fi

  # cargo-deny gates in CI beside this scan (CI-3, #148), and runs here for the reason the two blocks
  # above run here: a check that gates in CI and not locally retires this script's claim that a clean
  # run is a green gate. rust-doctor's own `dependencies (cargo-deny)` pass does not cover licences,
  # which is the half deny.toml made answerable at all.
  if command -v cargo-deny >/dev/null 2>&1; then
    deny_status=0
    deny_out="$(cargo deny check 2>&1)" || deny_status=$?
    if [ "$deny_status" -ne 0 ]; then
      printf '\n%s\n' "dependency policy (cargo-deny): WOULD FAIL — this gates in CI."
      printf '%s\n' "$deny_out" | grep -E '^(error|warning)' | sed 's/^/      /'
      verdict=3
    else
      printf '\n%s\n' "dependency policy (cargo-deny): would pass."
    fi
  else
    printf '\n%s\n' "not looked at here — cargo-deny is not installed, and it GATES in CI."
    printf '%s\n' "      A clean run above is therefore not a green gate. Install it with"
    printf '%s\n' "      \`cargo install cargo-deny --locked\` (or \`taiki-e/install-action\`, as CI does)."
  fi

  # typos gates in CI beside this scan (DOC-14, #144), and runs here for the same reason the three
  # blocks above do. Only the finding lines are echoed: typos prints the offending line and a caret per
  # finding, and this list is meant to be the thing you act on.
  if command -v typos >/dev/null 2>&1; then
    typos_status=0
    typos_out="$(typos --format brief 2>&1)" || typos_status=$?
    if [ "$typos_status" -ne 0 ]; then
      printf '\n%s\n' "spelling (typos): WOULD FAIL — this gates in CI."
      printf '%s\n' "$typos_out" | sed 's/^/      /'
      verdict=3
    else
      printf '\n%s\n' "spelling (typos): would pass."
    fi
  else
    printf '\n%s\n' "not looked at here — typos is not installed, and it GATES in CI."
    printf '%s\n' "      A clean run above is therefore not a green gate. Install it with"
    printf '%s\n' "      \`cargo install typos-cli --locked\` (the action's tool name is \`typos\`)."
  fi

  # LINT-6 (#146). `core.hooksPath` is per-clone: a commit cannot set it, so the checked-in hooks in
  # .githooks/ do nothing until someone opts in. This says so rather than staying quiet about it, which
  # is the same rule the sections above apply to skipped passes — an unconfigured guard reports nothing,
  # which reads exactly like a guard that found nothing.
  #
  # It does NOT set the verdict. The hooks are a convenience that moves two failures earlier; the gate
  # is this script and CI, and neither depends on them. A missing hook is not a finding about the code.
  if [ "$(git config core.hooksPath 2>/dev/null || true)" != ".githooks" ]; then
    printf '\n%s\n' "git hooks: not enabled in this clone (this is a note, not a finding)."
    printf '%s\n' "      .githooks/ holds a pre-commit that runs \`cargo fmt --all --check\` and a commit-msg"
    printf '%s\n' "      that checks the subject against the vocabulary release-notes.py categorises on."
    printf '%s\n' "      Both only move a failure earlier; nothing here depends on them. Enable with:"
    printf '%s\n' "        git config core.hooksPath .githooks"
  fi

  # zizmor gates in CI beside this scan (CI-5, #150), and runs here for the same reason the blocks above do.
  #
  # A REAL `zizmor` ON PATH IS PREFERRED AND `uvx` IS THE FALLBACK, in that order on purpose. This used to
  # run `uvx zizmor` only, on the stated grounds that "uvx is how CI runs it too, so there is no separate
  # binary to get out of step". That was the opposite of the truth: GitHub's runners have no `uvx`, so CI's
  # step exited 127 on every run while this one reported "would pass" — because a dev box usually *does* have
  # `uvx`, which downloads zizmor on demand. The two verdicts disagreed for eight hours, and the Actions
  # outage that hid the red is the only reason it took that long to notice.
  #
  # CI now installs zizmor through the same pinned install step as every other tool, so asking for the binary
  # first is asking the question CI answers. `uvx` stays as a convenience for a machine that has uv and not
  # zizmor, and the line says WHICH route ran — "would pass" from a different binary than the gate's is
  # precisely the claim that misled here. The label itself stays exactly "workflow lint (zizmor)" because
  # docs_claims.rs matches on it; the route goes in a trailing bracket rather than inside the parentheses.
  zizmor_via=""
  if command -v zizmor >/dev/null 2>&1; then
    zizmor_via="zizmor"
  elif command -v uvx >/dev/null 2>&1; then
    zizmor_via="uvx zizmor"
  fi
  if [ -n "$zizmor_via" ]; then
    zizmor_status=0
    zizmor_out="$($zizmor_via --persona=regular .github/ 2>&1)" || zizmor_status=$?
    if [ "$zizmor_status" -ne 0 ]; then
      printf '\n%s\n' "workflow lint (zizmor): WOULD FAIL — this gates in CI. [ran via ${zizmor_via}]"
      printf '%s\n' "$zizmor_out" | grep -E '^(error|warning)' | sed 's/^/      /'
      verdict=3
    else
      printf '\n%s\n' "workflow lint (zizmor): would pass. [ran via ${zizmor_via}]"
    fi
  else
    printf '\n%s\n' "not looked at here — no zizmor and no uvx, and zizmor GATES in CI."
    printf '%s\n' "      A clean run above is therefore not a green gate. Install the same binary CI uses:"
    printf '%s\n' "        cargo install zizmor        # or: uv tool install zizmor"
  fi

  # actionlint gates in CI beside zizmor (CI-9, #166), and runs here for the reason every block above runs
  # here: a check that gates in CI and not locally retires this script's claim that a clean run is a green
  # gate. The two workflow linters answer different questions and neither implies the other — zizmor audits
  # what a workflow is ALLOWED to do, actionlint whether it MEANS WHAT IT SAYS, which is what resolves
  # `needs.<job>.outputs.<name>` against the outputs that job declares.
  #
  # `-shellcheck=` and `-pyflakes=` are ON PURPOSE and must match the gate's invocation exactly; the reason
  # is written out at the step in rust-doctor.yml. In one line: those integrations are on by default when
  # the binaries are on PATH, GitHub's runners ship shellcheck and this box does not, so leaving them on is
  # a verdict that depends on which machine printed it.
  #
  # The VERSION is printed rather than assumed, the way the zizmor block prints its route. CI pins v1.7.12;
  # a local binary at some other version can find something the gate will not, and the line is what makes
  # that diagnosable instead of baffling.
  if command -v actionlint >/dev/null 2>&1; then
    actionlint_ver="$(actionlint --version 2>/dev/null | head -1)"
    actionlint_status=0
    actionlint_out="$(actionlint -no-color -oneline -shellcheck= -pyflakes= 2>&1)" || actionlint_status=$?
    if [ "$actionlint_status" -ne 0 ]; then
      printf '\n%s\n' "workflow semantics (actionlint): WOULD FAIL — this gates in CI. [local ${actionlint_ver}, CI pins 1.7.12]"
      printf '%s\n' "$actionlint_out" | sed 's/^/      /'
      verdict=3
    else
      printf '\n%s\n' "workflow semantics (actionlint): would pass. [local ${actionlint_ver}, CI pins 1.7.12]"
    fi
  else
    printf '\n%s\n' "not looked at here — actionlint is not installed, and it GATES in CI."
    printf '%s\n' "      A clean run above is therefore not a green gate. taiki-e/install-action does not"
    printf '%s\n' "      carry it, so CI curls the pinned release and you can do the same:"
    printf '%s\n' "        curl -fsSL https://github.com/rhysd/actionlint/releases/download/v1.7.12/actionlint_1.7.12_linux_amd64.tar.gz \\"
    printf '%s\n' "          | tar xz -C ~/.local/bin actionlint"
  fi

  # The CI scripts' fixture matrix gates in CI beside this scan (TEST-48, #163), and runs here for the
  # reason every block above runs here. Four Python scripts decide what CI publishes and none of them had
  # a test; `release-notes.py` shipping 13 commits with their type stripped is what that cost.
  #
  # Unlike cargo-shear or actionlint there is no third-party binary to be missing — python3 and git are
  # already hard requirements of this repo — but `run.sh` still reports a missing python3 itself and exits
  # non-zero, and that is echoed here rather than swallowed, because a matrix that did not run reports
  # nothing, which reads exactly like a matrix that found nothing.
  fixtures_status=0
  fixtures_out="$(bash scripts/tests/run.sh 2>&1)" || fixtures_status=$?
  if [ "$fixtures_status" -ne 0 ]; then
    printf '\n%s\n' "CI script fixtures (scripts/tests/run.sh): WOULD FAIL — this gates in CI."
    printf '%s\n' "$fixtures_out" | sed 's/^/      /'
    verdict=3
  else
    printf '\n%s\n' "CI script fixtures (scripts/tests/run.sh): would pass. [$(printf '%s' "$fixtures_out" | tail -1)]"
  fi

  exit "$verdict"
fi

# Modes whose stdout belongs to a program, not a person: straight through with no footer, and `exec` so
# this script does not sit in the middle of a long-lived one (`--mcp` serves stdio until it is killed).
for arg in "$@"; do
  case "$arg" in
  --json | --sarif | --score | --mcp) exec "$RUST_DOCTOR_BIN" "$@" ;;
  esac
done

status=0
"$RUST_DOCTOR_BIN" "$@" || status=$?

# The score card's headline is a weighted heuristic, and the gate is not weighted: one warning fails the
# build at any score. Those two facts have already been observed disagreeing on the same scan — 100/100
# "Great" over 21 warnings — so the box gets a footer saying which of the two numbers is the build.
cat >&2 <<EOF

note: that score is not the gate. CI fails the build on ANY warning, so "Great" can still be a red build —
      v0.2.0's tag build died exactly there, on five findings that were already in a local run and could
      not be read out of it (#42). For the findings behind those counts, and a pass/fail verdict:
        scripts/doctor.sh --findings
EOF
exit "$status"
