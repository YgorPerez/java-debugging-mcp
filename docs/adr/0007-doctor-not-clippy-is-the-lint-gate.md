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

**On `windows-gnu`, a local doctor run cannot verify the warning count — do not trust it.**

Doctor's isolated `target/rust-doctor` build fails to link there (`ld.exe: cannot find \symbols.o` — path
mangling in that separate build dir). It was first recorded as one cosmetic `error` to be read past. That
was wrong, and the first gated CI run proved it: **a build that cannot link is a clippy pass that cannot
run**, so a Windows doctor run reports only the custom AST rules (complexity, clone-in-loop) and silently
contributes *zero* clippy findings. It says "0 warnings" because it did not look, which is this repo's
recurring failure shape — an instrument reading healthy because it is measuring nothing.

What it cost: LINT-1 was verified locally at 0 warnings and pushed, and CI immediately failed the new
gate on three clippy findings that had been invisible on Windows — a `doc_markdown` in the integration
test crate (the exact blind spot this ADR is about) and `multiple_crate_versions` twice.

**Verify the clippy half locally with `cargo clippy` and doctor's own flags**, which works fine on
`windows-gnu` because it uses the normal target dir:

```
cargo clippy --workspace --all-targets -- -W clippy::pedantic -W clippy::nursery -W clippy::cargo
```

`--all-targets` is the load-bearing part — it is what reaches `tests/mcp_integration.rs`, and
`-W clippy::cargo` is what surfaces `multiple_crate_versions`. Neither is on by default, which is why
plain `cargo clippy` looked clean throughout. Run `scripts/doctor.sh` as well for the custom rules; on
Windows take the clippy findings from the command above and the rest from doctor.

## Amendment (LINT-2, #28) — who bumps the pin, and one clippy config instead of one per crate

The amendment above created two maintenance obligations and gave neither an owner. Both are settled here.

### The pin is bumped by a monthly advisory job that files an issue

`.github/workflows/toolchain-pin.yml` runs the same scan against whatever `stable` is now, once a month,
with `--fail-on none`. When stable differs from the pin it opens one issue — titled with the version, so
a re-run finds it rather than filing a second — carrying either the findings the newer clippy reports or
the news that there are none. Labelled `ready-for-agent` when the bump is a clean one-liner and
`needs-triage` when it is not.

This preserves the exact property the pin was chosen for: a new lint arrives as a notification, never as
a red build on code nobody touched. `schedule:` runs only on the default branch, produces no check run
on a pull request, and is not called by `release.yml`, so nothing can come to depend on its conclusion.

**The issue, rather than the run, is the notification** — because a scheduled job that goes red in a tab
nobody opens is the same silent staleness the job exists to fix. GitHub's two alternatives both fail
here: a run summary is only visible to someone who already went looking, and the failure email for a
scheduled run goes to whoever last edited the cron, which is not a role anyone holds. The tracker is
where this repo's work already lives.

**Rejected alternatives:**

- **Bump when it hurts** — raise the pin whenever someone wants a newer toolchain for something else.
  Zero process, unbounded staleness. It is also, precisely, the status quo that produced this issue.
- **A calendar reminder.** Cheap, but it lives outside the repo, so it survives exactly as long as one
  person's calendar does and tells a new maintainer nothing.
- **Dependabot / Renovate on the toolchain.** Most automatic, and it would open a PR whose CI answers
  the question directly. Rejected as the most noise for the least added information: the question is
  monthly, not per-commit, and a PR that fails the gate is the surprise breakage the pin exists to
  avoid — arriving in the shape of a broken build, which is what trains people to weaken a gate.

**Cost, stated plainly:** one more workflow, and up to a handful of issues a year. And a scheduled
workflow is disabled by GitHub after 60 days without repository activity, so a repo quiet for two
months has a staler pin than this job can fix.

### One `clippy.toml`, at the workspace root, plus `CLIPPY_CONF_DIR`

The two per-crate copies are gone. There is one `clippy.toml` at the root, and `scripts/doctor.sh`,
`rust-doctor.yml` and `toolchain-pin.yml` each set `CLIPPY_CONF_DIR` to the repo root. Two files kept in
sync by hand cannot drift apart when there is one file; a third crate needs nothing, because there is
nothing per-crate to add.

**The reason the copies existed was wrong, and the correction is the whole decision.** They carried a
note saying a root `clippy.toml` "works for `cargo clippy` but NOT for rust-doctor's invocation",
inferred from CI reporting the lint at `<crate>/clippy.toml:1` — "the path clippy looked in and did not
find". The symptom was real and reproduces: with only a root config, rust-doctor 0.2.0 reports
`multiple_crate_versions` once per member, while `cargo clippy --manifest-path <member>/Cargo.toml`
against the same tree reports nothing.

Shimming `cargo` under a doctor run shows why. **rust-doctor writes its own `<crate>/clippy.toml` into
any member that has none** — seven `allow-*-in-tests` keys — runs clippy, then deletes it. Clippy stops
at the first config it finds walking up from the crate, so that temporary file shadows the root one and
the `syn` allowance never applies. `<crate>/clippy.toml:1` was not a path clippy failed to find. It was
a file that existed for the length of the run, and the same mechanism would have shadowed any
workspace-level solution built on the walk-up.

`CLIPPY_CONF_DIR` points clippy at a directory instead of making it walk, so the injected file is never
consulted. Verified on Linux against rust-doctor 0.2.0 and Rust 1.97.1: root config alone, two
`multiple_crate_versions` warnings; root config plus `CLIPPY_CONF_DIR`, zero — and zero again with a
third workspace member added that carries no config of its own and, without the fix, fails the gate on
a duplication it did not cause.

**Nothing here relaxes the gate.** The root file carries the same two keys the copies did, so the
strictness is unchanged, and the `syn` allowance is still listed by name — the next duplicate is still a
finding. Note that suppressing the injected file also drops rust-doctor's own test allowances
(`allow-unwrap-in-tests` and five siblings); that costs nothing, because the copies already shadowed
them, which is presumably why `allow-panic-in-tests` had to be written out by hand in the first place.

**Rejected alternatives:**

- **Keep the two copies and add a check that they match and that every member has one.** Detects drift
  rather than preventing it, and still makes adding a crate a two-step operation someone has to be told
  about. A check that a hand-maintained duplicate is still duplicated is work spent guarding a shape
  there was no need to keep.
- **Symlink each member's `clippy.toml` to the root file.** One real file, no env var. Rejected because
  a Windows checkout without symlink support writes the link target as a text file, and clippy would
  then parse a path as TOML — on a repo whose ADRs already record `windows-gnu` reading healthy while
  measuring nothing.
- **Suppress the rule in `rust-doctor.toml`.** rust-doctor's own config can ignore findings. That
  silences the duplication everywhere instead of allowing one named crate, and is the "turn the gate
  off rather than fix the finding" move #18 exists to prevent.

**The failure mode if some future invocation forgets `CLIPPY_CONF_DIR`** is two `multiple_crate_versions`
warnings and a failed gate — loud, and in the safe direction. It cannot go quietly green.

## Amendment (LINT-3, #42) — the score is not the gate, and `--findings` is how you find out

The gate above only helps if you can see what it will fail on. Locally you could not. `scripts/doctor.sh`
ends in a summary box — `⚠ 5 warning(s)` — and the five findings behind that count were not reachable from
the run: grepping it for `⚠`, for `warning`, for the rule name, for `threshold` all returned nothing. The
mechanism, since it is worth knowing: the box goes to stdout, and what finding detail there is goes to
**stderr**, aggregated to one line per message with an occurrence count and no `file:line`. So capturing a
run the obvious way keeps the counts and discards the findings.

**It cost the v0.2.0 release.** The tag build passed the version check, all four platform builds and the
whole test suite, then failed this gate on five `excessive-clone` findings — every one of them already
present in a local run. The count had been watched going 1 → 5 and dismissed, because
`cargo clippy --all-targets` was clean (which is the *first* half of this ADR) and there was no cheap way
to see what the five **were**. They only became legible in CI's step summary, which renders the SARIF
properly. That is the tool that exists to catch things before CI, losing to CI at its own job.

**Decision:** `scripts/doctor.sh --findings` renders rust-doctor's `--json` in the same two-line shape the
workflow's step summary uses, so a finding reads identically wherever you meet it, and exits 3 when the
gate would fail. Parsed with `node`, which the script already hard-requires, rather than the workflow's
`python3`, which it does not: a findings mode that needs a second runtime is one that is missing when you
want it.

**And the score is not the gate.** It is a weighted heuristic; the gate is not weighted, and one warning
fails the build at any score. The two have been observed disagreeing on the same scan — **100/100
"Great" over 21 warnings** — so `--findings` prints a pass/fail verdict rather than a number, and the
plain run now carries a footer saying which of the two you are looking at.

**What the verdict is careful not to claim.** It is what the gate would say about *this scan*, not a
prediction of CI, and the mode prints both reasons that gap exists:

- **passes that did not run here** (the tool is not installed) — a pass that reports nothing reads exactly
  like a pass that found nothing, which is this repo's whole recurring failure shape;
- **passes that ran here and do not run in the gate** — the workflow installs no external tools, so a
  locally-installed `cargo-geiger` contributes `unsafe-dependency` warnings that CI will never see. That
  is not hypothetical either: it is where all 21 warnings in the 100/100 scan above came from.

Whether CI installs them is read out of the workflow rather than asserted here, for the same reason the
toolchain pin is: two copies of a fact drift.

**On the Windows `✗`:** it is now told apart by the **host triple**, not by matching the message, and the
warning says the half that is worth knowing — *your clippy findings are missing, not zero* — instead of
being one more always-present error line that teaches you to skip error lines. Written from this ADR's own
account of the failure; it has not been re-observed on a `windows-gnu` host since.
