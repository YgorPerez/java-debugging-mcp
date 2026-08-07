# The workflows

How CI is shaped, as opposed to what it checks — the gate's tools are in
[`lint-gate.md`](lint-gate.md) and the suite is in [`testing.md`](testing.md). Each workflow file carries
the same reasoning at its own lines; this is the summary you read before opening one.

## The test legs skip on a docs-only push, and the filtering is per-JOB

CI-6 (#151). That distinction is the whole issue: a workflow skipped by `on: push: paths:` produces **no
check run**, so a required status check never reports and a PR waits on something that will never arrive.

A `changes` job computes the answer with six lines of `git diff` — no third-party action to pin and let
Dependabot bump — and **`ci-ok` has no `if:`, so it reports on every event including the pushes where all
three legs skip**. That is the job branch protection would require; `main` has none today, so this is
groundwork rather than a change in enforcement.

**It fails open on every branch it cannot resolve**, and the release path is refused a filter *by ref,
checked first*: under `workflow_call` `github.event_name` is the caller's event, so a tag looks like an
ordinary push, and a **re-pointed** tag would have had a real `before` and let a release skip the suite
gating it. A newly created tag would have failed open by luck, and luck is not a gate — REL-1 (#34) is the
recorded cost.

## Every action is SHA-pinned, and Dependabot is the other half

CI-4 (#149), CI-5 (#150). The first zizmor run found **71**: 32 unpinned refs — two of them
`dtolnay/rust-toolchain@master`, a *branch* — 9 `artipacked` (checkout leaving credentials in the runner's
git config), a workflow-level `security-events: write` only one job needed, and several `${{ }}` expansions
inside `run:` blocks. **All fixed rather than suppressed, except one**: the `GITHUB_ENV` write in
`setup-rust`, which is what that step is, and which carries its reason at the line. If you accept a future
finding, annotate it at the line with why — a bare `# zizmor: ignore[…]` is how that becomes a file for
silencing the tool.

They land together because **a pin nobody bumps rots into a two-year-old `checkout`**, and
`dtolnay/rust-toolchain` is pinned to a bare commit because it publishes a tag per toolchain and has no
version tags at all. Both ecosystems are grouped weekly with a **7-day cooldown** — zizmor's
`dependabot-cooldown` audit caught its absence on the first run after the file was written, and it is the
point: pinning is undone by a bot that pulls a version the moment it exists. **Auto-merge is deliberately
off**, because `main` has no branch protection (the API returns 404), so there is nothing required to gate
on and auto-merge would mean merge-on-open.

## Every job has a `timeout-minutes`, derived at its own line

CI-10 (#172). It used to be one job in the whole repo, at a round 30; everything else ran under GitHub's
default of **360 minutes**. This suite's failure mode is a *wait* rather than an assertion — a probe JVM
that never reaches its breakpoint, a suspended debuggee nobody resumes — and with `fail-fast: false` the
bill was six hours of runner per hung leg.

**Each number says what it was derived from**, because a bare one rots the way a written-down shard number
does; where there is no measurement to derive from (the crates.io publish has never succeeded) the comment
says *that* instead of inventing one. The full argument lives once, at `tests.yml`'s `changes` job, and the
other files point at it.

Note the two `uses:` jobs in `release.yml` carry **no** timeout: the schema forbids it on a
reusable-workflow call, and actionlint fails the file on one. Their budget lives on the jobs inside the
called workflows.

## The toolkit parity workflow does not run on a schedule

CI-8 (#162). `scripts/toolkit-parity.py` diffs the downstream toolkit's documented tools against the
release it pins, and it **reports, never gates** — nothing here depends on the toolkit, and a check pointed
at another repo that could block a change is what got two scaffolded workflows deleted
([`lint-gate.md`](lint-gate.md)). It covers three of `docs/toolkit-contract.md`'s seven rows (a
renamed/removed tool, a renamed argument, an added tool nobody names) and says at its own head which four it
cannot. Every unresolvable input is **fatal**, because an empty diff from a run that read nothing is worse
than a red.

**There is no `schedule:`, and that is a deviation the issue's premise forced**:
`ygor-infotera/infotravel-dev-toolkit` is **private**, so the "public contents API" #162 assumed does not
exist and a workflow's `GITHUB_TOKEN` cannot reach it. Turning the schedule on means adding **this
repository's first secret** — `.github/workflows/toolkit-parity.yml` has the fine-grained scope and the
`gh secret set` recipe that keeps the token out of a transcript. Run it locally meanwhile: it shells out to
`gh api` and uses the auth you already have.

Measured 7 Aug 2026 at **40 tools named downstream, 40 exported, no drift in either direction** — and its
first run had a false positive worth knowing about, `debug.step_` from the glob `debug.step_*` in their
prose.
