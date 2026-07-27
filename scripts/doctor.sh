#!/usr/bin/env bash
#
# Local rust-doctor health check — the same tool the CI runs (arthjean/rust-doctor), pinned to the
# latest stable release. Uses `npx` to fetch a prebuilt native binary, so no Rust build of the tool
# is needed (it still shells out to your local `cargo clippy`). Requires Node/npx.
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
# Parsed with `node` rather than `python3` (which is what the workflow uses) because Node is already a hard
# requirement of this script — see the `npx` check at the top — and python3 is not. A findings mode that
# needs a second runtime is a findings mode that is not there when you want it.
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
  npx -y "rust-doctor@${RUST_DOCTOR_VERSION}" ${PASSTHRU[@]+"${PASSTHRU[@]}"} --json >"$SCAN" || status=$?
  if [ "$status" -ne 0 ] && [ "$status" -ne 3 ]; then
    echo "error: rust-doctor exited ${status} without completing a scan — no findings to report." >&2
    exit "$status"
  fi

  # Whether the gate's environment installs the optional external tools, read from the workflow rather than
  # asserted here, for the same reason the toolchain pin is read from it: two copies of a fact drift.
  CI_INSTALLS_TOOLS=0
  if grep -qE 'install-deps|cargo +install' .github/workflows/rust-doctor.yml 2>/dev/null; then
    CI_INSTALLS_TOOLS=1
  fi

  verdict=0
  SCAN_JSON="$SCAN" CI_INSTALLS_TOOLS="$CI_INSTALLS_TOOLS" node <<'JS' || verdict=$?
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
const localOnly =
  process.env.CI_INSTALLS_TOOLS === "0"
    ? ran.filter((p) => /\(cargo-[a-z-]+\)/.test(p) && !skipped.some((s) => s.startsWith(p)))
    : [];

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
  exit "$verdict"
fi

# Modes whose stdout belongs to a program, not a person: straight through with no footer, and `exec` so
# this script does not sit in the middle of a long-lived one (`--mcp` serves stdio until it is killed).
for arg in "$@"; do
  case "$arg" in
  --json | --sarif | --score | --mcp) exec npx -y "rust-doctor@${RUST_DOCTOR_VERSION}" "$@" ;;
  esac
done

status=0
npx -y "rust-doctor@${RUST_DOCTOR_VERSION}" "$@" || status=$?

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
