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

**Getting the JDKs CI has.** "More than one" was aspirational for a while because this workspace had only
JDK 11 and a snap JBR, so every result ended in "17 and 21 are CI's to confirm" — which is a slow way to
learn that a test is version-locked. Adoptium tarballs need no root and no package manager:

```bash
mkdir -p ~/.jdks && cd ~/.jdks
for v in 17 21; do
  curl -fsSL "https://api.adoptium.net/v3/binary/latest/$v/ga/linux/x64/jdk/hotspot/normal/eclipse" \
    | tar xz
done
JAVA_HOME=~/.jdks/jdk-17.0.20+8 scripts/integration-test.sh   # and quote the `JDK in use:` line
```

**A single test on an idle 16-core box is a *gentler* environment than CI, not a harsher one.** Worth
saying because two separate flake investigations here got it backwards and spent thousands of cycles
proving very little. CI runs all ~89 `#[ignore]`d tests at once on a 4-vCPU runner, so the contention that
produces these failures comes from dozens of probe JVMs competing, not from CPU scarcity alone. To
reproduce that, pin the whole suite instead of adding load:

```bash
taskset -c 0-3 cargo test --test mcp_integration -- --ignored --nocapture
```

Pass **no** `--test-threads`: libtest derives it from `available_parallelism()`, which honours CPU affinity
on Linux, so four cores make it choose four the way CI does. And prefer this to CPU hogs — a hog-based arm
leaked 32 processes twice, because `trap … EXIT` does not fire on SIGKILL.

**Run a soak against a copied binary, never the working tree.** `cp $(cargo test --no-run …) /tmp/arm.bin`
first. An arm that rebuilds while you edit reports *your compile errors* as failures: that produced a
confident "8 failures in 40" that were nothing of the kind.

## Agent skills

### Issue tracker

Issues and PRDs are tracked as GitHub issues (`YgorPerez/java-debugging-mcp`) via the `gh` CLI;
external PRs are not a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary — `needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`.
See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Downstream consumer

`infotravel-dev-toolkit` installs this server from a **pinned release** and documents its tools in Claude
Code skills. Nothing here depends on it and it can never break this CI — but five of its six failure modes
are silent, so a caller-visible change (a renamed tool or argument, a changed reply, new behaviour behind an
existing name) has to be stated in the release notes, and a behaviour change has to update the **tool
description** in the same commit. See `docs/toolkit-contract.md`.
