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

**There is deliberately no AI code-review workflow, and re-adding one by reflex would undo a decision.**
`/install-github-app` had scaffolded `claude-code-review.yml` and `claude.yml` — untouched template, commented-out
`paths:` filter and all. The auth step was never finished: the repo has **no secrets at all**, so
`CLAUDE_CODE_OAUTH_TOKEN` was always empty. The review workflow ran five times, **failed five times, and never
posted a comment**; the `@claude` one skipped twenty times and would have failed the same way if anyone had tried
it. Both are removed.

They were removed rather than wired up because a red check that verified nothing is the inversion of the rule the
rest of this section is built on. `--findings` names the passes that did not run, `sarif-for-code-scanning.py`
prints what it withheld, and `semver-check.sh` says "0 checks ran, so this verified nothing" instead of passing
quietly — all so a green tick cannot mean less than it looks like. A permanent red that tested nothing costs more
than that: it teaches you to ignore red on PRs. Review depth already comes from doctor (the gate, ADR-0007),
rust-doctor, the semver job, coverage, six integration legs, GitGuardian, and the `/code-review` skill locally —
which reads this file, `CONTEXT.md` and the ADRs rather than a generic five-bullet prompt. If an AI review is
wanted, it needs `CLAUDE_CODE_OAUTH_TOKEN` set as a repository secret **first**, and a prompt that names this
repo's actual risks (suspension honesty, resume verification, caller-visible replies) instead of "performance
considerations".

**Run the suite on more than one JDK.** `scripts/integration-test.sh` covers the `#[ignore]`d
JVM tests; plain `cargo test` covers the unit and cassette tests, and you need both to see all of
`mcp_integration.rs`. CI runs JDK 11/17/21 and has caught version-locked tests that passed on one JDK
(#36). Every run now prints one `JDK in use: …` line naming the version, the home the JVM reports, and
which of `JAVA_HOME` / `PATH` / the snap JBR it came from — read it, and quote it rather than your
intent when you report a result. Setting `JAVA_HOME` is a *request*: if it is not a usable JDK the run
now fails instead of quietly testing another one, which it used to do (TEST-18, #52).

**Every run ends with a ranked slowest-tests list, so quote it instead of estimating.** `scripts/test-timings.py`
prints it (TEST-26, #103; ADR-0024), and the three CI legs publish it into their job summaries. Two numbers,
easy to swap: **test time** is the sum of every test's own duration — occupancy, 647.4 s — and **wall clock**
is what you wait for, 177.3 s under `taskset -c 0-3`. Neither includes the build, the JDK install or the
cache restore. It exists because a triage estimate of the largest available saving was **4x too high**; the
same section warning you about the two backwards flake investigations applies to speed claims. As of v0.9.0
the four `*_is_honest_from_every_suspended_state` tests are **29% of the suite's test time** between them,
and the slowest single test is 74 s — which is the floor for anything that splits the suite up.

Because the timing flag is nightly-gated, `integration-test.sh` now **builds with `cargo test --no-run` and
runs the test binary directly**, keeping `RUSTC_BOOTSTRAP=1` off `cargo` — it is hashed into the build
fingerprint, so setting it on a `cargo test` recompiles the workspace and compiles it under a flag that lets
nightly-only features in silently. Arguments still go straight to libtest and the script still supplies the
`--`.

**CI runs six legs now: three JDKs x two shards** (TEST-29, #106; ADR-0025). A shard is half the suite split by
*measured* duration — `scripts/shard-plan.py` reading `mcp-server/tests/timings.tsv` — because a split by name
had a 1-in-8 chance of piling the four resume-honesty tests into one shard and making it the whole wall clock.
Measured on CI: **workflow wall clock 223 s → 147 s (−34%), runner-seconds 648 → 747 (+15%)**. Two shards and
not three for two reasons — the slowest single test is **70 s** and cannot be split, and a 60-test shard only
reaches ~2.6x concurrent on 4 vCPU against 3.7x for the full 118, so halving a shard's test time does *not*
halve its wall clock.

**Run the unsharded suite when you are working a flake.** `scripts/integration-test.sh` with no `--shard` still
runs all 118 tests in one process, which is the contention CI used to have. Sharding *reduces* how many probe
JVMs compete, so **a flake that stops reproducing under CI's new shape is not fixed** — #45, #56, #64 and #71
were open when this landed and the trade was accepted with that stated. Refresh the timings file with
`scripts/test-timings.py --emit-timings <log> > mcp-server/tests/timings.tsv`; it is generated, never hand-edited,
and drift is reported rather than fatal.

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
proving very little. CI runs all 117 `#[ignore]`d tests at once on a 4-vCPU runner, so the contention that
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

**The release body reaches the releases page through `scripts/release-notes.py`**, and it did not until
v0.9.0. The workflow published with `--generate-notes`, which lists merged **pull requests** — so a release
of direct pushes to main generated an empty "What's Changed", and the commit body it never read is where all
the caller-visible detail lives. Every release from v0.2.1 to v0.8.0 published one line: the compare link.
The script now leads with that commit body verbatim and appends a changelog categorized from the
conventional-commit subjects since the previous tag, under the same emoji headings `~/html/b2c-next` uses.
Preview it with `python3 scripts/release-notes.py v<version>`; it is byte-for-byte what will be published,
and it also lands in the run's job summary. There is deliberately **no `.github/release.yml`** — that is
b2c-next's mechanism and it categorizes by PR *label*, which here would categorize almost nothing and look
load-bearing while deciding nothing.

## Downstream consumer

`infotravel-dev-toolkit` installs this server from a **pinned release** and documents its tools in Claude
Code skills. Nothing here depends on it and it can never break this CI — but five of its six failure modes
are silent, so a caller-visible change (a renamed tool or argument, a changed reply, new behaviour behind an
existing name) has to be stated in the release notes, and a behaviour change has to update the **tool
description** in the same commit. See `docs/toolkit-contract.md`.
