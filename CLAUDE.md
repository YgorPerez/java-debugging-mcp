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
gate — machete's first run found `anyhow` and `serde_json` declared and unused by `jdwp-client`.

**`cargo-shear` is installed too, and it is not a rust-doctor pass — it is a step of its own, because
machete cannot see the case this workspace is shaped for.** Measured by planting one unused dependency in
each position and running both: in a *member crate* both flag it; named only in a *source comment* both
still flag it (machete is not the naive regex its reputation suggests — that was a guess, and it was
wrong); but an unused entry in the root **`[workspace.dependencies]`** table is reported by shear and
machete says "Good job!". Machete compares what a *package* declares against what that package's sources
use, and a workspace table is neither. Both members here take everything through `<name>.workspace = true`,
so an entry whose last user goes away is exactly the drift that would go unseen. Shear runs beside machete
rather than replacing it because the tool is *rust-doctor's* choice — the `dependencies` pass invokes
machete and takes no option to swap it — which is the same shape as the `semver` job below: a check
rust-doctor could not be pointed at the right thing for, living next to it instead of inside it.
`scripts/doctor.sh --findings` runs shear too, and **says so loudly when the binary is missing**, because a
local run that skips a check that gates in CI would retire this script's whole claim.

**That same script now also checks that the documentation renders**, so the instruction above already
covers it and there is no second command to learn. It gates in CI as a step beside rust-doctor (DOC-13,
#143) — the same shape as shear and the `semver` job, because rust-doctor has no rustdoc pass and takes no
option to add one. **The failure it catches is prose that compiles and does not render**, which is why it
earns its place in a repo where `rustfmt.toml` deliberately does not reflow comments so the narrative doc
comments can be written long: ~12,200 `///` and `//!` lines were a primary artefact that nothing checked.
It was red the day it was added, at **85 findings**. 74 were one cause — `JdwpError` linked from modules
that do not import it, so every `# Errors` section rendered a literal `[JdwpError]` — and the worst of the
other 11 was an unbackticked `Arc<Mutex<Receiver>>` that rustdoc parsed as an HTML tag, leaving the page
reading "wrapped in an Arc which allows sharing" with the type the sentence is about deleted from it.
`--document-private-items` is not optional: most of the narrative in `jdwp-client` hangs off private items,
and without it you check a small fraction of the prose you meant to. **Qualify a doc link rather than
widening visibility to satisfy one** — `mod_kinds::CLASS_ONLY` and `method_name_matches` are plain code
spans for that reason, and `pub(crate)` on the second was reverted because `clippy::redundant_pub_crate`
fails the gate on a `pub(crate)` item inside a private module. Two of the 85 were also **self-inflicted by
a careless bulk fix**: a regex that appended `(crate::X)` to every `[`X`]` doubled the target on the two
links that already had one, which rustdoc accepts as inert text and clippy catches as a bare path.

**`rust-toolchain.toml` now decides which compiler you are using, including locally** (LINT-5, #141).
`rustup` honours it, so `cargo fmt` and `scripts/doctor.sh` in this directory run the gate's pinned
toolchain without you arranging anything — which is the half of the problem CI configuration could never
reach. It replaces the sed that used to pull the number out of `rust-doctor.yml`; `scripts/doctor.sh` and
`toolchain-pin.yml` both read the file now, and the number lives in exactly one place. The old
`RUSTUP_TOOLCHAIN=… scripts/doctor.sh` recipe in `TODO.md` is history rather than instruction, and
doctor's "this run uses rustc X, but the gate is pinned to Y" warning is now a backstop that only fires
when something outranks the file — `RUSTUP_TOOLCHAIN`, a `+toolchain`, or a rustc rustup does not manage.

**The file outranks `dtolnay/rust-toolchain@stable`, so every job that wants a different toolchain says so
at its own call site**, through `.github/actions/setup-rust` (CI-7, #152), which collapsed seven
copy-pasted toolchain-plus-cache blocks across five workflows. The test, coverage and release legs pass
`stable` deliberately — they are the signal that the code still builds on current Rust, which a pinned
gate cannot give you — and the composite action **prints `Rust in use:` in every job**. Quote that line
rather than the `toolchain:` you asked for, for exactly the reason you quote `JDK in use:` rather than
your intent: a leg that asked for `stable`, silently got the pin, and passed is indistinguishable from one
that did what it said.

Three passes
stay off deliberately and `rust-doctor.yml` says why at each one: `cargo-geiger` feeds the
`unsafe-dependency` rule this repo ignores, `cargo-semver-checks` through *that* pass would compare against
**bonk-dev's** unrelated `jdwp-client` on crates.io (ours are unpublished) and answer confidently from the
wrong package — so the check lives in the `semver` job instead, via `scripts/semver-check.sh`, which uses the
last release **tag** as the baseline: 196 checks run that way against 0 through the pass. **It gates on the
release path only** (CI-2, #122): `release.yml` passes `gate_semver: true`, so a broken public API blocks a tag,
while a push to `main` or a PR gets the same finding printed — with the bump that would permit it — and
concludes green. That is a change, and the reason is that the red was routine: between releases the working
version *equals* the baseline tag, so every break violates the smallest bump and this job was the only failing
one in every run across two whole cycles. The only way to clear it was to cut the release. A permanent red that
tested nothing is the same defect as a green tick that means too much, and it is the one this file warns about
two paragraphs down. Read its verdict rather than the tick:
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

**A reworded reply is now a failing test, not a silent break** (TEST-46, #154). 163 substring
assertions guard the tool replies and a substring check cannot see a *rewording* — which is the failure
mode `docs/toolkit-contract.md` exists for, five of whose six downstream breakages are silent.
`mcp-server/tests/reply-fragments.txt` pins the output of the **pure** renderers (`describe_step_filter`,
`describe_trace_frames`, `describe_trace_exprs`, `merge_clamp_notes`, `describe_overridden_traces`) over
a fixed table of inputs, so it needs no JVM, no cassette and no redaction. **Fragments and not whole
replies, deliberately**: pinning everything makes each behaviour change a large diff and trains people
to regenerate without reading, which is the DOC-7 (#108) failure the generated file's own header warns
about. Regenerate with the **same** command as the other two snapshots —
`UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot` — and then **read the diff and put it in
the release notes**, because that is a caller-visible change.

**The wire read path is fuzzed, and the half that matters runs on stable** (TEST-45, #153).
`mcp-server/tests/malformed_wire.rs` rides on every `cargo test`: truncations and single-byte
corruptions of the **real frames in the cassettes**, all 256 value tags against every short buffer, and
lying string lengths. `fuzz/` is the deeper half — a **separate workspace** so nightly cannot reach the
crates the gate builds, invoked as `cargo +nightly fuzz run <target>` and **never** via
`RUSTC_BOOTSTRAP`. Its corpus is **generated** by `fuzz/seed-corpus.py` from those same cassettes rather
than committed, so re-recording a cassette cannot leave a stale copy behind. The claim under test is
*never panics*, not memory safety — there is no `unsafe` here; a panic in the event loop drops a session,
and a dropped session can leave a shared debuggee suspended. **It found nothing, which was the
expectation**: `reader.rs` already guards every read with `ensure` plus a checked slice. Verified by
planting a panic in `ReplyPacket::decode` and confirming two tests catch it — an unverified "no crashes"
result is indistinguishable from a harness that never ran.

**Seven of this file's claims are now asserted against the tree** (DOC-15, #145).
`mcp-server/tests/docs_claims.rs` runs in plain `cargo test` and checks the *claims* rather than the
code — that the pin is still readable by the sed two scripts use, that the first `tool:` line is the
health job's, that every check `doctor.sh` says "GATES in CI" is really a step in the gate. Most guard a
**sed rather than a number**, which is the cheapest thing here to get wrong: a sed that stops matching
returns *empty*, and empty reads like "nothing to report" everywhere it lands. The ignored-test count
lives in `mcp_integration.rs` as an `#[ignore]`d test, because only the binary's own `--list --ignored`
can answer it — a static `grep -c '#[ignore'` says 188 and `timings.tsv` has 222 rows, and all three are
honest. **It caught the drift on its first run**, and the deal it enforces is that a red here is fixed by
updating the number in the same commit. **Where that deal is not worth it, delete the number instead of
pinning it** — that is what happened to the second copy of this count, and to the `--shard N/M` rule,
where every occurrence in the tree is either a usage line or prose explaining why not to write one down,
so the test would only ever have fired on the documentation of its own rule.

**The test legs skip on a docs-only push, and the filtering is per-JOB rather than `on: push: paths:`**
(CI-6, #151). That distinction is the whole issue: a workflow skipped by a path filter produces **no
check run**, so a required status check never reports and a PR waits on something that will never
arrive. A `changes` job computes the answer with six lines of `git diff` — no third-party action to pin
and let Dependabot bump — and **`ci-ok` has no `if:`, so it reports on every event including the pushes
where all three legs skip**. That is the job branch protection would require; `main` has none today, so
this is groundwork rather than a change in enforcement. **It fails open on every branch it cannot
resolve**, and the release path is refused a filter *by ref, checked first*: under `workflow_call`
`github.event_name` is the caller's event, so a tag looks like an ordinary push, and a **re-pointed**
tag would have had a real `before` and let a release skip the suite gating it. A newly created tag would
have failed open by luck, and luck is not a gate — REL-1 (#34) is the recorded cost.

**Every action is SHA-pinned and `zizmor` gates the workflows** (CI-4/#149, CI-5/#150). The first
zizmor run found **71**: 32 unpinned refs — two of them `dtolnay/rust-toolchain@master`, a *branch* —
9 `artipacked` (checkout leaving credentials in the runner's git config), a workflow-level
`security-events: write` only one job needed, and several `${{ }}` expansions inside `run:` blocks.
**All fixed rather than suppressed, except one**: the `GITHUB_ENV` write in `setup-rust`, which is what
that step is, and which carries its reason at the line. If you accept a future finding, annotate it at
the line with why — a bare `# zizmor: ignore[…]` is how that becomes a file for silencing the tool.
Dependabot is the other half and they land together: **a pin nobody bumps rots into a two-year-old
`checkout`**, and `dtolnay/rust-toolchain` is pinned to a bare commit because it publishes a tag per
toolchain and has no version tags at all. Both ecosystems are grouped weekly with a **7-day cooldown** —
zizmor's `dependabot-cooldown` audit caught its absence on the first run after the file was written,
and it is the point: pinning is undone by a bot that pulls a version the moment it exists. **Auto-merge
is deliberately off**, because `main` has no branch protection (the API returns 404), so there is
nothing required to gate on and auto-merge would mean merge-on-open.

**Two git hooks are checked in, and they do nothing until you opt in** (LINT-6/#146, REL-4/#147):
`git config core.hooksPath .githooks` — per-clone, because a commit cannot set it. `pre-commit` runs
`cargo fmt --all --check`; `commit-msg` checks the subject against the types `release-notes.py`
categorises on, reading them from `--list-types` rather than a second copy, which is why there is no
commitlint config here. `scripts/doctor.sh --findings` prints a note when the config is unset, and
`bash .githooks/test.sh` is the 22-case matrix. **Two thirds of those cases assert the hooks do not
fire**, for the reason the `.claude/` guard's matrix gives: a hook that rejects a subject the maintainer
writes gets uninstalled the same day. Its history-replay case earned that immediately — the first
version rejected 23 of this repo's own subjects, because `merge:` was missing from the vocabulary and
the compound `fix(lint)+docs:` form failed the regex. That second one was a live defect in the published
notes, not just the hook: those 13 commits had been landing in "Other Changes" with their type stripped.

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
the four `*_is_honest_from_every_suspended_state` tests were **29% of the suite's test time** between them,
and the slowest single test was 74 s. **TEST-30 cut that**: they were waiting out a 25 s `EVENT_TIMEOUT` to
observe an *absence* — proving "this path correctly did not resume" by letting the wait expire — and reading
the path's own `STILL suspended` admission instead took the suite from **609 s to 540 s of test time** and the
floor from **70 s to 35 s**. The lesson generalises past this suite: **a negative observation needs only as
long as the positive would have taken.** `WatchProbe` ticks every 150 ms, so 25 s was two orders of magnitude
more than ruling it out required.

**TEST-35 then removed the floor altogether, and the cause was not the one the timings suggested.** The five
resume-honesty tests each looped over eight suspended states *inside one test body*, and every cell launches its
own probe JVM and its own server — so eight JVMs ran **sequentially in one libtest thread** while the other
fifteen sat idle. Per-cell instrumentation found each cell waiting ~2 s and advancing, so barely half of the
35 s was the assertions at all; the unshortenable 25 s wait in the watchdog cell was the obvious suspect and was
not the cost. Splitting them into **40 per-cell `#[test]`s** took the slowest single test to **18.7 s** and
shard 1/2 on 4 vCPU from **53.7 s to 30.7 s**, for no extra runners. **Check whether a slow test can be
*scheduled* before looking at the timeouts inside it** — `shard-plan.py` cannot split one test, so a long loop
is a floor under every shard count.

**The suite is oversubscribed on purpose, and the runner prints the number** (TEST-32). Almost every test
waits on a probe JVM rather than computing, so libtest's `available_parallelism()` default leaves the cores
idle. Measured on 4 vCPU: **4 threads 139.1 s (3.8x concurrent), 8 threads 87.7 s (6.3x), 16 threads 63.8 s
(10.2x)** — *ten times concurrent on four cores*. Sixteen cores continue it: 16 -> 56.9 s, 24 -> 50.1 s,
40 -> 45.6 s. `integration-test.sh` therefore runs **4x cores capped at 40**, and `JDWP_TEST_THREADS`
overrides.

**Do not re-derive this from Brent's bound** (`total_work / threads`), which says four threads is already
97 % efficient and is how the lever stayed invisible: that bound assumes CPU-bound work, and this suite is
not. Neither is copying a neighbour's number the answer — `b2c-next`'s vitest takes one worker per core and
`~/html/infotravel-doc` caps Playwright at `workers: 4`, both correct for CPU-bound JS. The transferable
part is *do not accept a default that assumes work you do not have*.

**It raises contention, which is what the flakes come from.** Accepted deliberately: the trade is stated in
TEST-32's commit and #114 carries the one flake the soak surfaced.

Because the timing flag is nightly-gated, `integration-test.sh` now **builds with `cargo test --no-run` and
runs the test binary directly**, keeping `RUSTC_BOOTSTRAP=1` off `cargo` — it is hashed into the build
fingerprint, so setting it on a `cargo test` recompiles the workspace and compiles it under a flag that lets
nightly-only features in silently. Arguments still go straight to libtest and the script still supplies the
`--`.

**CI runs six legs now: three JDKs x two shards** (TEST-29, #106; ADR-0025). A shard is half the suite split by
*measured* duration — `scripts/shard-plan.py` reading `mcp-server/tests/timings.tsv` — because a split by name
had a 1-in-8 chance of piling the four resume-honesty tests into one shard and making it the whole wall clock.
Measured on CI when sharding landed: **workflow wall clock 223 s → 147 s (−34%), runner-seconds 648 → 747
(+15%)** — wall clock bought *at the cost of* runner-seconds. **Re-measured after TEST-30 and TEST-32: wall
clock 91 s, slowest leg 88 s, runner-seconds 494 s**, so both are now better than either earlier state and
runner-seconds are below the *unsharded* baseline. **33 s of that 88 s leg is not the tests** (16 s build,
the rest checkout/toolchain/cache), so fixed cost per leg is what now argues against a third shard. Two shards and
not three for two reasons — the slowest single test was **70 s** and cannot be split, and a 60-test shard only
reaches ~2.6x concurrent on 4 vCPU against 3.7x for the full 118, so halving a shard's test time does *not*
halve its wall clock. **Both reasons have now expired, and the answer did not
change.** TEST-30 took the floor to 35 s and TEST-35 removed it (18.7 s, and that one is a single test rather
than a loop). Re-measured on 4 vCPU with no floor left: **shard 1/2 = 30.7 s at 14.0x concurrent, shard 1/3 =
30.6 s at 11.6x** — falling concurrency cancels the smaller workload exactly, so a third shard is worth
**0.1 s** for +50 % runners. Still two, now on a measurement rather than a caveat; ADR-0025 carries both
amendments. Note what moved the needle each time: a *test* got faster, never the shard arithmetic.

**Run the unsharded suite when you are working a flake.** `scripts/integration-test.sh` with no `--shard` still
runs every `#[ignore]`d test in one process — **219** as of 2026-08-06, and DOC-15 (#145) now asserts that number against the binary rather than asking you to trust it — which is the contention CI used to have.
Count it rather than trusting that number (`<the-test-binary> --list --ignored | grep -c ': test$'`); this line
said **164** for long enough that the figure was wrong by a third. Sharding *reduces* how many probe
JVMs compete, so **a flake that stops reproducing under CI's new shape is not fixed** — #45, #56, #64 and #71
were open when this landed and the trade was accepted with that stated. Refresh the timings file with
`scripts/test-timings.py --emit-timings <log> > mcp-server/tests/timings.tsv`; it is generated, never hand-edited,
and drift is reported rather than fatal.

**A shard number written down anywhere is stale, and following one costs a green run of nothing.** #118's
reproduction recipe named `--shard 1/2`; six runs of it passed cleanly and the test it was supposed to
exercise had moved to shard **2/2**, because the split is by *measured* duration and the suite had grown from
180 tests to 197 with `timings.tsv` refreshed several times in between. Six green runs that proved nothing and
looked like they proved something.

*The suite has grown again since, so those two numbers are now history as well — which is the point rather
than an aside: this paragraph's own figures rotted in the weeks it took to read it. The count above is
carried in one place and asserted (DOC-15, #145); this sentence deliberately names no number, because a
second copy of it is what rotted here twice.* **Check membership before
trusting a shard number:**

```bash
scripts/shard-plan.py --tests <(<the-test-binary> --ignored --list) --which launch_suspends
# 2/2  launch_suspends_before_the_first_instruction_and_disconnect_terminates_it
```

`--which` exits non-zero and says so when the name is in **no** shard, which is the case that otherwise looks
like a pass. Prefer the unsharded form in anything you write down: it has no shard number to rot.

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
proving very little. CI runs all 164 `#[ignore]`d tests at once on a 4-vCPU runner, so the contention that
produces these failures comes from dozens of probe JVMs competing, not from CPU scarcity alone. To
reproduce that, pin the whole suite instead of adding load:

```bash
taskset -c 0-3 cargo test --test mcp_integration -- --ignored --nocapture
```

Pass **no** `--test-threads`: `integration-test.sh` now computes one (**4x cores, capped at 40** — TEST-32)
and prints it, and under `taskset -c 0-3` that comes out at **16**, which is exactly what CI passes. So the
recipe still reproduces CI's contention; it just is not libtest's default any more. Overriding with
`JDWP_TEST_THREADS`, or an explicit `--test-threads`, changes the shape and stops it being CI's.

And prefer this to CPU hogs — a hog-based arm leaked 32 processes twice, because `trap … EXIT` does not
fire on SIGKILL.

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

### Guardrails

**Some of the traps above are now enforced rather than described.** `.claude/hooks/pre-bash-guard.py`
runs on every Bash call and denies `RUSTC_BOOTSTRAP=1 cargo …` and a `git commit` over a misformatted
tree, asks before `git push`, and warns on a soak loop against the working tree, a hardcoded
`--shard N/M`, a `--test-threads` override, and unbounded workspace cargo output. Every rule is one
already written down in this file — the hook adds no policy, it just stops the policy depending on
somebody having read this far.

**Two of those rules used to search the raw command line**, which the hook's own module docstring says
not to do and for exactly the reason it gives: commands here routinely *mention* the guarded thing as
data. Recording a soak result with `gh issue comment` made the guard fire on the prose it was writing
about itself, and a rule that cries wolf on its own documentation is the fastest way to get the whole
guard switched off. Both now walk **tokens** and both require the *value* to follow — `--test-threads`
needs a digit and `--shard` needs `N/M` — so a `grep` for the flag in the docs is not an override.
Three of the matrix's cases are that regression, and they were found in the wild rather than imagined.

The rationale for each severity lives in `.claude/settings.json`'s comment block and is deliberately
**not** repeated here; that file is the one place to change it. `bash .claude/hooks/pre-bash-guard.test.sh`
is the 20-case matrix, and eleven of those cases assert the guard does *not* fire — a guard that trips
on a heredoc or an `echo` gets switched off within the day. Escape any deny with
`SKIP_JDWP_AGENT_GUARD=1` in the command itself.

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
