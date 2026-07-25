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
- Two warnings are known and deliberately left, both recorded in `TODO.md`: the `syn` 2.x/3.x duplication
  from `schemars` vs `serde_derive`, and four pre-existing handlers over the cyclomatic-complexity
  threshold (verified unchanged against `main`, so not a regression).
