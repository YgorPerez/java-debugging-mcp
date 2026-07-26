# 0007 — `scripts/doctor.sh`, not `cargo clippy`, is the lint gate

## Context

The clippy policy lives as `#![warn(clippy::pedantic, clippy::nursery, …)]` crate attributes in
`jdwp-client/src/lib.rs` and `mcp-server/src/main.rs`. `Cargo.toml` explains why it is there rather than in
a `[lints]` table: the CI gate passes its flags on the command line, which a manifest table cannot override.

What that note does not say, and what cost several commits: **crate attributes apply to their own crate**.
`mcp-server/tests/mcp_integration.rs` is a *separate* crate with no such attributes, so
`cargo clippy --workspace --all-targets` reports **zero** warnings on it no matter what is in it.

rust-doctor passes the lint flags on the command line, so it does cover the test crate. Nine real warnings
had accumulated there — `i64 as usize` casts that could truncate and lose sign, redundant clones, missing
doc backticks — across commits each reported as "clippy clean".

## Decision

`scripts/doctor.sh` is the gate. Run it before calling a change clean; `scripts/doctor.sh --diff main`
scopes it to what you changed. `cargo clippy` remains useful as a fast inner-loop check on the two library
crates, and nothing more.

## Rejected alternative

Adding the crate attributes to the top of the integration test file. It would work, but it puts the policy
in a third place that can drift from the other two, and it would still not cover a *new* test file that
forgot them. The gate already lints everything; the fix is to run the gate.

## Consequences

- "clippy clean" is not a meaningful claim about this repo. Cite the doctor score instead.

## Amendment (LINT-1, #18) — the gate fails on warnings, on a pinned toolchain

This ADR originally closed by recording two warnings as deliberately left: the `syn` 2.x/3.x duplication
from `schemars` vs `serde_derive`, and "four pre-existing handlers over the cyclomatic-complexity
threshold (verified unchanged against `main`, so not a regression)".

**That second claim was wrong**, and worth recording as the reason this amendment exists. `7253499`
reached zero warnings deliberately. Twenty-four commits later `main` was back to seven, and the four
over-threshold handlers were described as pre-existing debt the new work merely "matched" — when they were
a regression from a state this repo had already paid for. Nothing enforced the zero, so nothing noticed;
the check was "whoever remembers to look", and the memory of *having cleared it* was what decayed first.

The gate is now `--fail-on warning` on a **pinned** toolchain (`.github/workflows/rust-doctor.yml`).

**Rejected alternatives:**

- **Gate on warnings with `stable`.** Simplest, and the reason the `--fail-on error` compromise existed:
  a new pedantic lint in a future clippy breaks CI on code nobody touched, which trains people to weaken
  the gate rather than fix the finding. Pinning turns that from a surprise into a scheduled bump.
- **Gate on a committed warning count.** Catches regressions without making a new upstream lint an
  emergency, but needs a checked-in baseline that is itself a thing to keep honest — and a baseline above
  zero invites raising it by one.
- **Stay ungated.** Defensible: the score is a heuristic, and the finding list was always the value rather
  than the number. Rejected because the drift above is what "ungated" actually looked like in practice —
  not a considered trade, but a silent one that was then mis-reported as pre-existing.

Note the deliberate contrast with coverage, where `TODO.md` records a decision **not** to gate on a
percentage. The difference is what the number means: a coverage percentage can be raised by tests that
assert nothing, so the gate would measure the wrong thing, whereas a doctor warning is a specific finding
at a specific line that is either fixed or not.

**Cost, stated plainly:** the pin means the repo lints against one clippy until someone raises it, so new
upstream lints are found on a bump rather than on the day they ship. That is the trade — a gate that holds,
against warnings arriving later.

**Environmental caveat:** on a `windows-gnu` toolchain, doctor's isolated `target/rust-doctor` build fails
to link (`ld.exe: cannot find \symbols.o` — path mangling in that separate build dir) and reports one
`error`. The normal `cargo build` and `cargo clippy` are clean, and CI runs on Linux and does not hit it.
Locally on Windows, read the warning count and ignore that error.
