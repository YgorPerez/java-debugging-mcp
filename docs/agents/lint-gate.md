# The lint gate

Everything behind `scripts/doctor.sh --findings`. **The instruction to run it lives in `CLAUDE.md`**; this
file is why each check is there, which is a fact you look up rather than one you need before you act.

Every entry below was written because something got past the gate. None of it is a preference.

## There is no baseline any more: a clean tree prints `would pass`, so any finding is yours

It used to be "21 `unsafe-dependency` findings, anything beyond that is yours" — a pass condition you had to
decode against a number written down in a doc, and one that was never constant, since it is whatever subset
of the dependency tree `cargo-geiger` flags and it moves with every `Cargo.lock` change. Those findings were
all about *third-party* crates (`tokio`, `syn`, `serde_json`…), none of which anyone was going to replace,
and CI never even ran the pass. `rust-doctor.toml` now ignores the rule and explains it at length, including
why the `Warning: unknown rule(s) in ignore config` the tool prints on every run is **false** — the entry
works, and was measured working. Our own `unsafe` is a different rule and still fails the gate.

## GitHub's security tab shows only what the gate fails on

Via `scripts/sarif-for-code-scanning.py`, and an empty tab there is now meaningful rather than reassuring.
It used to publish rust-doctor's raw SARIF, which grew to **115 open alerts against a green gate**: 109
`excessive-clone` notes (one identical sentence, on `Arc` handle clones), 6 `skipped-pass` notes (a tool
wasn't installed — not a finding about the code), and every one of them anchored to a path that does not
exist in this tree, because rust-doctor writes crate-relative URIs (`src/handlers.rs`) under a `%SRCROOT%`
base id it never declares and this is a workspace. The script resolves the paths, publishes
`warning`/`error` only, and prints what it withheld into the job summary — so nothing is silently dropped.
Notes still reach you two other ways: the full SARIF is the `rust-doctor-sarif` artifact, and `--findings`
prints locally.

## The tools the gate installs, and why each one is separate

**CI installs `cargo-deny` and `cargo-machete`** (prebuilt, seconds), so those two passes are part of the
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

**Doctor also checks that the documentation renders**, so the one instruction covers it and there is no
second command to learn. It gates in CI as a step beside rust-doctor (DOC-13, #143) — the same shape as
shear and the `semver` job, because rust-doctor has no rustdoc pass and takes no option to add one. **The
failure it catches is prose that compiles and does not render**, which is why it earns its place in a repo
where `rustfmt.toml` deliberately does not reflow comments so the narrative doc comments can be written
long: ~12,200 `///` and `//!` lines were a primary artefact that nothing checked. It was red the day it was
added, at **85 findings**. 74 were one cause — `JdwpError` linked from modules that do not import it, so
every `# Errors` section rendered a literal `[JdwpError]` — and the worst of the other 11 was an
unbackticked `Arc<Mutex<Receiver>>` that rustdoc parsed as an HTML tag, leaving the page reading "wrapped in
an Arc which allows sharing" with the type the sentence is about deleted from it. `--document-private-items`
is not optional: most of the narrative in `jdwp-client` hangs off private items, and without it you check a
small fraction of the prose you meant to. **Qualify a doc link rather than widening visibility to satisfy
one** — `mod_kinds::CLASS_ONLY` and `method_name_matches` are plain code spans for that reason, and
`pub(crate)` on the second was reverted because `clippy::redundant_pub_crate` fails the gate on a
`pub(crate)` item inside a private module. Two of the 85 were also **self-inflicted by a careless bulk
fix**: a regex that appended `(crate::X)` to every `[`X`]` doubled the target on the two links that already
had one, which rustdoc accepts as inert text and clippy catches as a bare path.

**`actionlint` gates beside `zizmor`, and the two answer different questions** (CI-9, #166). zizmor audits
what a workflow is *allowed to do*; actionlint checks whether it *means what it says* — chiefly by resolving
`needs.<job>.outputs.<name>` against the outputs that job declares. That is the check CI-6 (#151) needs:
`needs.changes.outputs.rust == 'true'` appears four times in `tests.yml`, a typo evaluates to the empty
string, every leg skips, and **`ci-ok` reports green by design**. **Its first run found 0**, which is the
result and is not a reason to soften the claim — this one is a bet on the next rename. Two things about its
invocation are decisions rather than defaults: it is **curled from a pinned release** because
`taiki-e/install-action` does not carry it, and **`-shellcheck=` / `-pyflakes=` turn off integrations that
are on whenever those binaries are on `PATH`**. GitHub's runners ship shellcheck and a dev box may not, so
leaving them on makes the verdict depend on which machine printed it — measured at 3 findings under
shellcheck 0.11.0, none of them a semantics finding. `scripts/doctor.sh --findings` runs the identical
invocation and prints both versions, and `docs_claims.rs` asserts the two files name the same one.

## The toolchain pin

**`rust-toolchain.toml` decides which compiler you are using, including locally** (LINT-5, #141). `rustup`
honours it, so `cargo fmt` and `scripts/doctor.sh` in this directory run the gate's pinned toolchain without
you arranging anything — which is the half of the problem CI configuration could never reach. It replaces
the sed that used to pull the number out of `rust-doctor.yml`; `scripts/doctor.sh` and `toolchain-pin.yml`
both read the file now, and the number lives in exactly one place. Doctor's "this run uses rustc X, but the
gate is pinned to Y" warning is a backstop that only fires when something outranks the file —
`RUSTUP_TOOLCHAIN`, a `+toolchain`, or a rustc rustup does not manage.

**The file outranks `dtolnay/rust-toolchain@stable`, so every job that wants a different toolchain says so
at its own call site**, through `.github/actions/setup-rust` (CI-7, #152), which collapsed seven
copy-pasted toolchain-plus-cache blocks across five workflows. The test, coverage and release legs pass
`stable` deliberately — they are the signal that the code still builds on current Rust, which a pinned gate
cannot give you. **`CLAUDE.md` carries the rule that follows from this**: quote the action's `Rust in use:`
line rather than the `toolchain:` you asked for.

## Three passes stay off deliberately

`rust-doctor.yml` says why at each one.

- **`cargo-geiger`** feeds the `unsafe-dependency` rule this repo ignores (above).
- **`cargo-semver-checks`** through *that* pass would compare against the latest crates.io **release**,
  which cannot answer for a commit between releases — and before REL-5 it was wrong more loudly than that,
  comparing against **bonk-dev's** unrelated `jdwp-client`, the name our library has since been renamed away
  from. So the check lives in the `semver` job instead, via `scripts/semver-check.sh`, which uses the last
  release **tag** as the baseline: 196 checks run that way against 0 through the pass.
- **coverage** belongs to `coverage.yml`.

`--findings` works the "ran here, but not in the gate" question out **per tool** from the workflow's install
list, so it stays true as that list changes — it used to be a yes/no grep for `cargo install` anywhere in
the file, which a *comment* containing those words silently flipped.

**The `semver` job gates on the release path only** (CI-2, #122): `release.yml` passes `gate_semver: true`,
so a broken public API blocks a tag, while a push to `main` or a PR gets the same finding printed — with the
bump that would permit it — and concludes green. That is a change, and the reason is that the red was
routine: between releases the working version *equals* the baseline tag, so every break violates the
smallest bump and this job was the only failing one in every run across two whole cycles. The only way to
clear it was to cut the release. A permanent red that tested nothing is the same defect as a green tick that
means too much. Read its verdict rather than the tick: on a release commit every check skips, because a bump
that permits breaking changes leaves nothing to violate, and the script prints "0 checks ran, so this
verified nothing" instead of passing quietly.

## There is deliberately no AI code-review workflow

**Re-adding one by reflex would undo a decision.** `/install-github-app` had scaffolded
`claude-code-review.yml` and `claude.yml` — untouched template, commented-out `paths:` filter and all. The
auth step was never finished: the repo has **no secrets at all**, so `CLAUDE_CODE_OAUTH_TOKEN` was always
empty. The review workflow ran five times, **failed five times, and never posted a comment**; the `@claude`
one skipped twenty times and would have failed the same way if anyone had tried it. Both are removed.

They were removed rather than wired up because a red check that verified nothing is the inversion of the
rule the rest of the gate is built on. `--findings` names the passes that did not run,
`sarif-for-code-scanning.py` prints what it withheld, and `semver-check.sh` says "0 checks ran, so this
verified nothing" instead of passing quietly — all so a green tick cannot mean less than it looks like. A
permanent red that tested nothing costs more than that: it teaches you to ignore red on PRs. Review depth
already comes from doctor (the gate, ADR-0007), rust-doctor, the semver job, coverage, six integration legs,
GitGuardian, and the `/code-review` skill locally — which reads `CLAUDE.md`, `CONTEXT.md` and the ADRs
rather than a generic five-bullet prompt. If an AI review is wanted, it needs `CLAUDE_CODE_OAUTH_TOKEN` set
as a repository secret **first**, and a prompt that names this repo's actual risks (suspension honesty,
resume verification, caller-visible replies) instead of "performance considerations".
