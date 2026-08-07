# java-debugging-mcp

A native JDWP debugger exposed as an MCP server (Rust). `jdwp-client` speaks the JDWP wire protocol;
`mcp-server` wraps it as `debug.*` MCP tools (attach, breakpoints, stack/variable inspection,
expression evaluation, stepping). See `README.md` for the full tool list and setup.

**This file is always loaded, so it holds constraints and routes and nothing else** (DOC-17, #169). The
test for a paragraph earning a place here is whether **reading it after you have acted is too late** — the
traps below all cost time exactly that way. Everything that is a fact you can look up when you need it
lives behind a route, and `docs/agents/task-map.md` says which route for which task.

## Where to read next

| you are about to | read |
|---|---|
| pick what to read | `docs/agents/task-map.md` |
| use the vocabulary a caller sees | `CONTEXT.md` |
| understand why something is the way it is | `docs/adr/` |
| change a gate tool | `docs/agents/lint-gate.md` |
| work on the suite, or a flake | `docs/agents/testing.md` |
| change a workflow | `docs/agents/ci.md` |
| cut a release | `/release`, then `docs/agents/releasing.md` |
| set up a clone, or the git hooks | `docs/development.md` |
| triage | `docs/agents/issue-tracker.md`, `docs/agents/triage-labels.md` |

## Before you commit

**Run `cargo fmt`.** The workspace is rustfmt-formatted as of LINT-4 (#44) and CI fails on a misformatted
diff. Settings live in `rustfmt.toml` and were measured off this tree rather than chosen — comments are
*not* reflowed, so the narrative doc comments are safe to write long.

**Run `scripts/doctor.sh --findings`**, not just `cargo clippy`. Doctor is the gate (ADR-0007), it fails on
*warnings*, and the score is not the verdict — 100/100 "Great" has sat on top of 21 warnings. `--findings`
prints what the gate will fail on and says whether it would pass. **A clean tree prints `would pass`, so any
finding is yours**; it also runs every check that gates in CI beside rust-doctor and says loudly when a
binary it needs is missing, because a local run that skips one would retire its whole claim. What each check
is for: `docs/agents/lint-gate.md`.

**Quote what a run printed, not what you asked for.** Every test leg prints `JDK in use: …` and every job
prints `Rust in use: …`. A leg that asked for `stable`, silently got the pin, and passed is
indistinguishable from one that did what it said — and the same is true of a JDK. This has been real twice.

## Naming, and what a caller reads

**Check `CONTEXT.md` before naming anything a caller will read**, and **do not call it `inherited`** — that
word is taken on this surface for a field walked from a superclass (`list_fields {inherited:true}`,
ADR-0015). It shipped in #134's replies anyway and lasted a day: one word doing two unrelated
caller-visible jobs. The glossary carries **Session default** with that collision in its `_Avoid_` line.

**A caller-visible change goes in the release notes, and a behaviour change updates the tool description in
the same commit.** `docs/toolkit-contract.md` is why: six of the seven ways a change here reaches the
downstream toolkit are silent, and the notes are the only mitigation for most of them.

**Regenerate a snapshot with `UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot`, then read the
diff.** One command rewrites all three snapshots. Reading the diff *is* the mechanism — DOC-7 (#108)
shipped interleaved gibberish in a release because nobody read a 4000-character line. The same rule applies
to `scripts/tests/run.sh --update`.

**A `String` returned by a helper only reveals its rendering when you read the rendered output.** Two
replies embedded a multi-line indented block inside a sentence and the compiler was happy;
`reply-fragments.txt` exists to keep that found.

## Running the suite

**Run more than one JDK.** `scripts/integration-test.sh` covers the `#[ignore]`d JVM tests; plain
`cargo test` covers the unit and cassette tests, and you need both to see all of `mcp_integration.rs`. CI
runs 11/17/21 and has caught version-locked tests that passed on one (#36). `docs/agents/testing.md` has
how to get those JDKs locally.

**Do not re-derive the thread count from Brent's bound** (`total_work / threads`), which says four threads
is already 97 % efficient and is how a ten-times-concurrent lever stayed invisible: that bound assumes
CPU-bound work, and this suite waits on probe JVMs. Neither is copying a neighbour's number the answer. The
transferable part is *do not accept a default that assumes work you do not have*. The measurements are in
`docs/agents/testing.md`; `integration-test.sh` computes the number and prints it, so you need not.

**A single test on an idle 16-core box is a *gentler* environment than CI, not a harsher one.** Two separate
flake investigations here got that backwards and spent thousands of cycles proving very little. The
contention comes from dozens of probe JVMs competing, so reproduce it by pinning the whole suite rather than
by adding load:

```bash
taskset -c 0-3 cargo test --test mcp_integration -- --ignored --nocapture
```

Pass **no** `--test-threads`, and prefer this to a CPU hog — a hog-based arm leaked 32 processes twice,
because `trap … EXIT` does not fire on SIGKILL.

**A shard number written down anywhere is stale, and following one costs a green run of nothing.** #118's
recipe named `--shard 1/2`; six runs of it passed cleanly and the test had moved to shard 2/2, because the
split is by *measured* duration. Six green runs that proved nothing and looked like they proved something.
Check membership rather than trusting a number:

```bash
scripts/shard-plan.py --tests <(<the-test-binary> --ignored --list) --which launch_suspends
```

`--which` exits non-zero when the name is in **no** shard, which is the case that otherwise looks like a
pass. Prefer the unsharded form in anything you write down: it has no number to rot.

**Run a soak against a copied binary, never the working tree.** `cp $(cargo test --no-run …) /tmp/arm.bin`
first. An arm that rebuilds while you edit reports *your compile errors* as failures: that produced a
confident "8 failures in 40" that were nothing of the kind.

**Adding a JDWP command to `jdwp-client` costs a line in a table, including a read** (SAFE-12, #171).
`WIRE_COMMANDS` in `connection.rs`'s test module classifies **every** command this crate can send as `Read`,
`Mutation` or `AllowedStateChange`, and the suite goes red on one it has never heard of. That is the point:
ADR-0001 says read-only is enforced *at the wire*, and SAFE-9 (#60) is the record of that invariant breaking
with nothing failing. Do not reach for a third verdict to make a red go away — `VirtualMachine.Suspend` is
an `AllowedStateChange`, not a read, and the enum has that value so nobody has to lie.

## Agent skills

### Issue tracker

Issues and PRDs are tracked as GitHub issues (`YgorPerez/java-debugging-mcp`) via the `gh` CLI;
external PRs are not a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary — `needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`.
See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Guardrails

**Some of the traps above are enforced rather than described.** `scripts/guard.py` denies
`RUSTC_BOOTSTRAP=1 cargo …` and a `git commit` over a misformatted tree, asks before `git push`, and warns
on a soak loop against the working tree, a hardcoded `--shard N/M`, a `--test-threads` override, and
unbounded workspace cargo output. Every rule is one already written down here — the guard adds no policy, it
just stops the policy depending on somebody having read this far. Escape any deny with
`SKIP_JDWP_AGENT_GUARD=1` in the command itself.

**Nothing runs it for you outside Claude Code** (LINT-7, #167). The rules are host-neutral now and
`.claude/hooks/pre-bash-guard.py` is an adapter over them, but only that host invokes it automatically; the
two checked-in git hooks cover the `cargo fmt` half of one rule and are opt-in per clone besides. Elsewhere
it is a command:

```bash
scripts/guard.py check 'RUSTC_BOOTSTRAP=1 cargo test'   # allow | warn | ask | deny, with the reason
```

The rationale for each severity, and the matrix's case counts, live in `.claude/settings.json`'s comment
block — one place, and `docs_claims.rs` asserts the counts. `bash scripts/guard.test.sh` is the matrix, most
of whose cases assert the guard does *not* fire: a guard that trips on a heredoc or an `echo` gets switched
off within the day.

## Releasing

**Use `/release [X.Y.Z]`** (`.claude/commands/release.md`). `scripts/release.sh` is the half that bumps,
gates, commits and tags — deliberately stopping before the push — and the command is everything around it:
which bump, the release body (which *is* the release notes, so the toolkit can read it), the tag push, the
downstream pin and skill audit, and the issue closes. **Do not hand-roll a release.**

It leads with the four traps that have actually cost time, so read them rather than rediscovering them. The
worst is that a non-interactive `release.sh` writes only the commit **subject**, and repairing that means
**re-tagging** — an annotated tag names one commit, and amending leaves the tag pointing at an object no
longer on the branch.

**A tag publishes to crates.io, which is the one step nothing can undo** — a version can be yanked but keeps
its number forever. It runs last for that reason. What else a tag publishes, and why each piece is shaped
the way it is: `docs/agents/releasing.md`.

**Anything taking a `-p` package name wants `java-debugging-jdwp-client`, not `jdwp-client`.** The obvious
name belongs to an unrelated project on crates.io. `[lib] name` is still `jdwp_client`, so imports are
untouched — but `cargo clean -p jdwp-client` cleans nothing and exits 0, which left the stale-cache step in
`rust-doctor.yml` **vacuous** while it still read as passing.

## Downstream consumer

`infotravel-dev-toolkit` installs this server from a **pinned release** and documents its tools in Claude
Code skills. Nothing here depends on it and it can never break this CI — but five of its six failure modes
are silent, so a caller-visible change (a renamed tool or argument, a changed reply, new behaviour behind an
existing name) has to be stated in the release notes, and a behaviour change has to update the **tool
description** in the same commit. See `docs/toolkit-contract.md`.
