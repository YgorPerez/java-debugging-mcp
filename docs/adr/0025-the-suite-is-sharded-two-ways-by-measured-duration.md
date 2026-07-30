# 0025 — The integration suite is sharded two ways per JDK, split by measured duration

**Status:** accepted (TEST-29, #106)

## Decision

Each JDK leg runs **half** the integration suite. Six legs instead of three, the split computed from
`mcp-server/tests/timings.tsv` by `scripts/shard-plan.py` using greedy longest-processing-time, expressed to
libtest as an explicit name list under `--exact`.

**Two shards, not three.** And the split is by measured duration, not by name.

## Why this reversed the expectation

#106 was filed `needs-triage` arguing against itself, and expected to close `wontfix` on the grounds that
per-leg fixed cost might dominate. Measured, it does not:

| | measured |
|---|---|
| test execution per leg | **188 s** |
| everything else per leg | 28 s (setup 3, checkout 1, toolchain 0–3, cargo cache 4–6, JDK install **0**, version assert 2–3, build 17) |
| execution as a share of a leg | **87%** |

The "nine JDK installs and nine cache restores" cost the issue worried about is not what bites — `setup-java`
measures 0 s on these runners because the image already ships the JDKs, and the shared `rust-cache` key makes
the restore 4–6 s.

TEST-27 (#104) is closed `wontfix` for a related reason worth stating here so the two are not re-litigated
together: its 17 s of warm build is real, but hoisting it into a shared job serialises what the three legs do
concurrently. Sharding is the opposite shape — it adds *parallel* legs, and pays.

## Why by measured duration and not by name

A name-based split is stable and trivial, and it would have been the wrong choice for a reason #103's ranking
made visible: the four `*_is_honest_from_every_suspended_state` tests are **29% of the suite's test time**
between them (188.5 s of 647 s). A hash-of-name split has roughly a 1-in-8 chance of landing all four in one
shard, and that shard *is* the workflow's wall clock — so the failure mode is not "slightly unbalanced", it is
"sharding appears not to work, intermittently, depending on the test names in the tree".

By duration the plan comes out at **283 s / 284 s** of test time. Measured leg wall clock: 79 s and 52 s.

The cost of choosing duration is a committed snapshot that drifts. That is handled by reporting rather than by
pretending it cannot happen: a test with no recorded duration is charged the median **and named on stderr**,
which lands in the CI log; a recorded duration for a test that no longer exists is named too. Coverage is never
affected by drift — the plan partitions whatever `--list` returns.

## Why two shards and not three

The slowest single test is **70 s** (`resume_thread_is_honest_from_every_suspended_state`) and a test cannot be
split, so it is a floor. Shard 1 measures 79 s of wall clock with that 70 s test inside it — already within 9 s
of the floor. A third shard divides test time that the floor already dominates: the projection is ~106 s per
leg against ~115 s for two shards, for three more runners per push.

So two shards is not a cautious first step towards more. It is where this stops being worth anything.

## The risk, accepted with the trade stated

Fewer tests per shard means **fewer probe JVMs contending at once**, and `CLAUDE.md` records two flake
investigations that reasoned backwards about exactly that variable. #45, #56, #64 and #71 are open. This change
could plausibly make some of them stop reproducing without being fixed — and they would return the first time
anyone rebalanced a shard.

#106 argued for sequencing the flakes first on that basis. The maintainer chose to take the wall-clock win now,
with the risk named. Two things follow, and both are load-bearing:

- **A flake investigation must run the unsharded suite.** `scripts/integration-test.sh` with no `--shard` still
  runs all 118 tests in one process, which is the contention CI used to have. `CLAUDE.md`'s soak instructions
  are unchanged and still correct.
- **A flake that stops reproducing after this is not evidence it is fixed.** It is evidence the contention
  changed. Anyone closing one of those four needs a reason that does not rest on a green sharded run.

## The fourth green-run-of-nothing guard

Sharding opened a gap the existing three could not see: a leg that runs *fewer* tests than its shard selected
is green over tests nobody executed, and the SKIP grep, the `0 passed` check and the `JDK in use:` check all
pass it.

Two checks, because neither is sufficient alone:

- `scripts/shard-plan.py` refuses a plan that is not a partition of the test list — nothing may fall between
  shards or land in two.
- The workflow asserts per leg that the number of tests **run** equals the number **selected**. That is the half
  a single leg can know; the partition check is the half it cannot, since a leg cannot see whether its sibling
  ran.

## Consequences

- Six legs per push. A red leg now names a JDK *and* a shard, which is strictly more attribution than before.
- `mcp-server/tests/timings.tsv` is a committed generated file. Refresh it with
  `scripts/test-timings.py --emit-timings <log> > mcp-server/tests/timings.tsv` when the ranking changes
  materially — not every run, and never by hand.
- Failed tests are excluded from the timings file: a failure's duration is when it gave up, usually a timeout,
  which is the largest and least representative number in the run.
