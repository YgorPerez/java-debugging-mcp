# java-debugging-mcp

A native JDWP debugger exposed as an MCP server (Rust). `jdwp-client` speaks the JDWP wire protocol;
`mcp-server` wraps it as `debug.*` MCP tools (attach, breakpoints, stack/variable inspection,
expression evaluation, stepping). See `README.md` for the full tool list and setup.

## Before you commit

**Run `cargo fmt`.** The workspace is rustfmt-formatted as of LINT-4 (#44) and CI fails on a
misformatted diff. Settings live in `rustfmt.toml` and were measured off this tree rather than
chosen — comments are *not* reflowed, so the narrative doc comments are safe to write long.

**Run `scripts/doctor.sh --findings`**, not just `cargo clippy`. Doctor is the gate (ADR-0007), it
fails on *warnings*, and the score is not the verdict — 100/100 "Great" has sat on top of 21 warnings.
`--findings` prints what the gate will fail on and says whether it would pass. The baseline is 21
`unsafe-dependency` findings from a locally-installed `cargo-geiger` that CI does not install; anything
beyond that baseline is yours.

**Run the suite on more than one JDK.** `scripts/integration-test.sh` covers the `#[ignore]`d
JVM tests; plain `cargo test` covers the unit and cassette tests, and you need both to see all of
`mcp_integration.rs`. CI runs JDK 11/17/21 and has caught version-locked tests that passed on one JDK
(#36). Every run now prints one `JDK in use: …` line naming the version, the home the JVM reports, and
which of `JAVA_HOME` / `PATH` / the snap JBR it came from — read it, and quote it rather than your
intent when you report a result. Setting `JAVA_HOME` is a *request*: if it is not a usable JDK the run
now fails instead of quietly testing another one, which it used to do (TEST-18, #52).

## Agent skills

### Issue tracker

Issues and PRDs are tracked as GitHub issues (`YgorPerez/java-debugging-mcp`) via the `gh` CLI;
external PRs are not a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary — `needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`.
See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
