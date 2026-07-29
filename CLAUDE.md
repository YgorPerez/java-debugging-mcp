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
`--findings` prints what the gate will fail on and says whether it would pass.

**There is no baseline any more: a clean tree prints `would pass`, so any finding is yours.** It used to
be "21 `unsafe-dependency` findings, anything beyond that is yours" — a pass condition you had to decode
against a number written down here, and one that was never constant, since it is whatever subset of the
dependency tree `cargo-geiger` flags and it moves with every `Cargo.lock` change. Those findings were all
about *third-party* crates (`tokio`, `syn`, `serde_json`…), none of which anyone was going to replace, and
CI never even ran the pass. `rust-doctor.toml` now ignores the rule and explains it at length, including
why the `Warning: unknown rule(s) in ignore config` the tool prints on every run is **false** — the entry
works, and was measured working. Our own `unsafe` is a different rule and still fails the gate.

**GitHub's security tab shows only what the gate fails on**, via `scripts/sarif-for-code-scanning.py`, and an
empty tab there is now meaningful rather than reassuring. It used to publish rust-doctor's raw SARIF, which
grew to **115 open alerts against a green gate**: 109 `excessive-clone` notes (one identical sentence, on
`Arc` handle clones), 6 `skipped-pass` notes (a tool wasn't installed — not a finding about the code), and
every one of them anchored to a path that does not exist in this tree, because rust-doctor writes
crate-relative URIs (`src/handlers.rs`) under a `%SRCROOT%` base id it never declares and this is a
workspace. The script resolves the paths, publishes `warning`/`error` only, and prints what it withheld into
the job summary — so nothing is silently dropped. Notes still reach you two other ways: the full SARIF is
the `rust-doctor-sarif` artifact, and `--findings` prints locally.

**CI now installs `cargo-deny` and `cargo-machete`** (prebuilt, seconds), so those two passes are part of the
gate — machete's first run found `anyhow` and `serde_json` declared and unused by `jdwp-client`. Three passes
stay off deliberately and `rust-doctor.yml` says why at each one: `cargo-geiger` feeds the
`unsafe-dependency` rule this repo ignores, `cargo-semver-checks` through *that* pass would compare against
**bonk-dev's** unrelated `jdwp-client` on crates.io (ours are unpublished) and answer confidently from the
wrong package — so the check lives in the `semver` job instead, via `scripts/semver-check.sh`, which uses the
last release **tag** as the baseline: 196 checks run that way against 0 through the pass. It gates, and
`release.yml` calls this workflow, so a broken public API blocks a tag. Read its verdict rather than the tick:
on a release commit every check skips, because a bump that permits breaking changes leaves nothing to
violate, and the script prints "0 checks ran, so this verified nothing" instead of passing quietly. Coverage
belongs to `coverage.yml`. `--findings` works this out **per tool** from the workflow's install
list, so "ran here, but not in the gate" stays true as that list changes — it used to be a yes/no grep for
`cargo install` anywhere in the file, which a *comment* containing those words silently flipped.

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

## Releasing

**Use `/release [X.Y.Z]`** (`.claude/commands/release.md`). `scripts/release.sh` is the half that bumps,
gates, commits and tags — deliberately stopping before the push — and the command is everything around it:
which bump, the release body (which *is* the release notes, so the toolkit can read it), the tag push, the
downstream pin and skill audit, and the issue closes.

It leads with the four traps that have actually cost time, so read them rather than rediscovering them. The
worst is that a non-interactive `release.sh` writes only the commit **subject**, and repairing that means
**re-tagging** — an annotated tag names one commit, and amending leaves the tag pointing at an object no
longer on the branch.

## Downstream consumer

`infotravel-dev-toolkit` installs this server from a **pinned release** and documents its tools in Claude
Code skills. Nothing here depends on it and it can never break this CI — but five of its six failure modes
are silent, so a caller-visible change (a renamed tool or argument, a changed reply, new behaviour behind an
existing name) has to be stated in the release notes, and a behaviour change has to update the **tool
description** in the same commit. See `docs/toolkit-contract.md`.
