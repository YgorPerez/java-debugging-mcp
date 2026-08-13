# The test suite

What is tested, how it is scheduled, and how to reproduce a failure. **`CLAUDE.md` carries the rules that
have to be read before you act** — do not re-derive the thread count, do not trust a written-down shard
number, do not soak against the working tree. This file is the catalogue behind them.

## The two layers, and why you need both

`scripts/integration-test.sh` covers the `#[ignore]`d JVM tests; plain `cargo test` covers the unit and
cassette tests. You need both to see all of `mcp_integration.rs`. CI runs JDK 11/17/21 and has caught
version-locked tests that passed on one JDK (#36).

Setting `JAVA_HOME` is a *request*: if it is not a usable JDK the run now fails instead of quietly testing
another one, which it used to do (TEST-18, #52).

### Getting the JDKs CI has

"More than one" was aspirational for a while because this workspace had only JDK 11 and a snap JBR, so
every result ended in "17 and 21 are CI's to confirm" — which is a slow way to learn that a test is
version-locked. Adoptium tarballs need no root and no package manager:

```bash
mkdir -p ~/.jdks && cd ~/.jdks
for v in 17 21; do
  curl -fsSL "https://api.adoptium.net/v3/binary/latest/$v/ga/linux/x64/jdk/hotspot/normal/eclipse" \
    | tar xz
done
JAVA_HOME=~/.jdks/jdk-17.0.20+8 scripts/integration-test.sh   # and quote the `JDK in use:` line
```

## What the JVM-free tests cover

**A reworded reply is a failing test, not a silent break** (TEST-46, #154). 163 substring assertions guard
the tool replies and a substring check cannot see a *rewording* — which is the failure mode
`docs/toolkit-contract.md` exists for, five of whose six downstream breakages are silent.
`mcp-server/tests/reply-fragments.txt` pins the output of the **pure** renderers (`describe_step_filter`,
`describe_trace_frames`, `describe_trace_exprs`, `merge_clamp_notes`, `describe_overridden_traces`) over a
fixed table of inputs, so it needs no JVM, no cassette and no redaction. **Fragments and not whole replies,
deliberately**: pinning everything makes each behaviour change a large diff and trains people to regenerate
without reading, which is the DOC-7 (#108) failure the generated file's own header warns about.

**The four Python scripts that decide what CI publishes have a fixture matrix** (TEST-48, #163).
`bash scripts/tests/run.sh` — one committed transcript per case holding the command, the **exit status**,
and **stdout and stderr separately**, because that split is part of the contract (`shard-plan.py` puts names
on stdout so a caller can pipe them) and a merged capture would let it swap silently. It gates in CI beside
rust-doctor, `scripts/doctor.sh --findings` runs the identical command, and there is a `--update` that you
must **read the diff of**. **The cost is already on the board**: `release-notes.py` did not match the
compound `fix(lint)+docs:` subject, so **13 commits reached published release notes with their type
stripped**, and the commit-msg hook found it rather than anything testing the script. Every case was
verified by **reverting the behaviour it covers**.

**The wire read path is fuzzed, and the half that matters runs on stable** (TEST-45, #153).
`mcp-server/tests/malformed_wire.rs` rides on every `cargo test`: truncations and single-byte corruptions of
the **real frames in the cassettes**, all 256 value tags against every short buffer, and lying string
lengths. `fuzz/` is the deeper half — a **separate workspace** so nightly cannot reach the crates the gate
builds, invoked as `cargo +nightly fuzz run <target>` and **never** via `RUSTC_BOOTSTRAP`. Its corpus is
**generated** by `fuzz/seed-corpus.py` from those same cassettes rather than committed, so re-recording a
cassette cannot leave a stale copy behind. The claim under test is *never panics*, not memory safety —
there is no `unsafe` here; a panic in the event loop drops a session, and a dropped session can leave a
shared debuggee suspended. **It found nothing, which was the expectation**: `reader.rs` already guards every
read with `ensure` plus a checked slice. Verified by planting a panic in `ReplyPacket::decode` and
confirming two tests catch it — an unverified "no crashes" result is indistinguishable from a harness that
never ran.

**Ten of this repo's claims are asserted against the tree** (DOC-15, #145).
`mcp-server/tests/docs_claims.rs` runs in plain `cargo test` and checks the *claims* rather than the code —
that the pin is still readable by the sed two scripts use, that the first `tool:` line is the health job's,
that every check `doctor.sh` says "GATES in CI" is really a step in the gate. Most guard a **sed rather than
a number**, which is the cheapest thing here to get wrong: a sed that stops matching returns *empty*, and
empty reads like "nothing to report" everywhere it lands. The ignored-test count is the exception and lives
in `mcp_integration.rs`, because only the binary's own `--list --ignored` can answer it — a static
`grep -c '#[ignore'` says 188 and `timings.tsv` has 222 rows, and all three are honest. **It caught the
drift on its first run**, and the deal it enforces is that a red here is fixed by updating the number in the
same commit. **Where that deal is not worth it, delete the number instead of pinning it** — that is what
happened to the second copy of this count, and to the `--shard N/M` rule, where every occurrence in the tree
is either a usage line or prose explaining why not to write one down, so the test would only ever have fired
on the documentation of its own rule.

## Timings, and the two numbers that are easy to swap

**Every run ends with a ranked slowest-tests list, so quote it instead of estimating.**
`scripts/test-timings.py` prints it (TEST-26, #103; ADR-0024), and the three CI legs publish it into their
job summaries. **Test time** is the sum of every test's own duration — occupancy, 647.4 s — and **wall
clock** is what you wait for, 177.3 s under `taskset -c 0-3`. Neither includes the build, the JDK install or
the cache restore. It exists because a triage estimate of the largest available saving was **4x too high**;
the same warning about the two backwards flake investigations applies to speed claims.

As of v0.9.0 the four `*_is_honest_from_every_suspended_state` tests were **29% of the suite's test time**
between them, and the slowest single test was 74 s. **TEST-30 cut that**: they were waiting out a 25 s
`EVENT_TIMEOUT` to observe an *absence* — proving "this path correctly did not resume" by letting the wait
expire — and reading the path's own `STILL suspended` admission instead took the suite from **609 s to
540 s of test time** and the floor from **70 s to 35 s**. The lesson generalises past this suite: **a
negative observation needs only as long as the positive would have taken.** `WatchProbe` ticks every 150 ms,
so 25 s was two orders of magnitude more than ruling it out required.

**TEST-35 then removed the floor altogether, and the cause was not the one the timings suggested.** The five
resume-honesty tests each looped over eight suspended states *inside one test body*, and every cell launches
its own probe JVM and its own server — so eight JVMs ran **sequentially in one libtest thread** while the
other fifteen sat idle. Per-cell instrumentation found each cell waiting ~2 s and advancing, so barely half
of the 35 s was the assertions at all; the unshortenable 25 s wait in the watchdog cell was the obvious
suspect and was not the cost. Splitting them into **40 per-cell `#[test]`s** took the slowest single test to
**18.7 s** and shard 1/2 on 4 vCPU from **53.7 s to 30.7 s**, for no extra runners. **Check whether a slow
test can be *scheduled* before looking at the timeouts inside it** — `shard-plan.py` cannot split one test,
so a long loop is a floor under every shard count.

## Threads: the suite is oversubscribed on purpose

**And the runner prints the number** (TEST-32). Almost every test waits on a probe JVM rather than
computing, so libtest's `available_parallelism()` default leaves the cores idle. Measured on 4 vCPU:
**4 threads 139.1 s (3.8x concurrent), 8 threads 87.7 s (6.3x), 16 threads 63.8 s (10.2x)** — *ten times
concurrent on four cores*. Sixteen cores continue it: 16 -> 56.9 s, 24 -> 50.1 s, 40 -> 45.6 s.
`integration-test.sh` therefore runs **4x cores capped at 40**, and `JDWP_TEST_THREADS` overrides.

**`CLAUDE.md` carries the rule this produced**, and does not restate it here. What belongs here is the
evidence for the other half of it — copying a neighbour's number is no better than re-deriving one:
`b2c-next`'s vitest takes one worker per core and `~/html/infotravel-doc` caps Playwright at `workers: 4`,
both correct for CPU-bound JS and both wrong for a suite that waits on JVMs.

**It raises contention, which is what the flakes come from.** Accepted deliberately: the trade is stated in
TEST-32's commit and #114 carries the one flake the soak surfaced.

Because the timing flag is nightly-gated, `integration-test.sh` **builds with `cargo test --no-run` and runs
the test binary directly**, keeping `RUSTC_BOOTSTRAP=1` off `cargo` — it is hashed into the build
fingerprint, so setting it on a `cargo test` recompiles the workspace and compiles it under a flag that lets
nightly-only features in silently. Arguments still go straight to libtest and the script still supplies the
`--`.

## Sharding: six legs, three JDKs x two shards

TEST-29 (#106); ADR-0025. A shard is half the suite split by *measured* duration —
`scripts/shard-plan.py` reading `mcp-server/tests/timings.tsv` — because a split by name had a 1-in-8 chance
of piling the four resume-honesty tests into one shard and making it the whole wall clock.

Measured on CI when sharding landed: **workflow wall clock 223 s → 147 s (−34%), runner-seconds 648 → 747
(+15%)** — wall clock bought *at the cost of* runner-seconds. **Re-measured after TEST-30 and TEST-32: wall
clock 91 s, slowest leg 88 s, runner-seconds 494 s**, so both are now better than either earlier state and
runner-seconds are below the *unsharded* baseline. **33 s of that 88 s leg is not the tests** (16 s build,
the rest checkout/toolchain/cache), so fixed cost per leg is what now argues against a third shard.

Two shards and not three for two reasons — the slowest single test was **70 s** and cannot be split, and a
60-test shard only reaches ~2.6x concurrent on 4 vCPU against 3.7x for the full 118, so halving a shard's
test time does *not* halve its wall clock. **Both reasons have now expired, and the answer did not change.**
TEST-30 took the floor to 35 s and TEST-35 removed it (18.7 s, and that one is a single test rather than a
loop). Re-measured on 4 vCPU with no floor left: **shard 1/2 = 30.7 s at 14.0x concurrent, shard 1/3 =
30.6 s at 11.6x** — falling concurrency cancels the smaller workload exactly, so a third shard is worth
**0.1 s** for +50 % runners. Still two, now on a measurement rather than a caveat; ADR-0025 carries both
amendments. Note what moved the needle each time: a *test* got faster, never the shard arithmetic.

## Working a flake

**Run the unsharded suite.** `scripts/integration-test.sh` with no `--shard` still
runs every `#[ignore]`d test in one process — **232** as of 2026-08-13 — which is the contention CI used to
have. DOC-15 (#145) asserts that number against the binary rather than asking you to trust it; the
assertion lives in `mcp_integration.rs` and anchors on the sentence above, **so keep it on one line** — it
was broken across two by DOC-17's move and the test caught it immediately. Count it yourself if you prefer
(`<the-test-binary> --list --ignored | grep -c ': test$'`); this line said **164** for long enough that the
figure was wrong by a third.

Sharding *reduces* how many probe JVMs compete, so **a flake that stops reproducing under CI's new shape is
not fixed** — #45, #56, #64 and #71 were open when this landed and the trade was accepted with that stated.
Refresh the timings file with
`scripts/test-timings.py --emit-timings <log> > mcp-server/tests/timings.tsv`; it is generated, never
hand-edited, and drift is reported rather than fatal.

### Why a written-down shard number is stale

#118's reproduction recipe named `--shard 1/2`; six runs of it passed cleanly and the test it was supposed
to exercise had moved to shard **2/2**, because the split is by *measured* duration and the suite had grown
from 180 tests to 197 with `timings.tsv` refreshed several times in between. Six green runs that proved
nothing and looked like they proved something.

*The suite has grown again since, so those two numbers are now history as well — which is the point rather
than an aside: that paragraph's own figures rotted in the weeks it took to read it. The count above is
carried in one place and asserted (DOC-15, #145).* Check membership before trusting a shard number:

```bash
scripts/shard-plan.py --tests <(<the-test-binary> --ignored --list) --which launch_suspends
# 2/2  launch_suspends_before_the_first_instruction_and_disconnect_terminates_it
```

`--which` exits non-zero and says so when the name is in **no** shard, which is the case that otherwise
looks like a pass. Prefer the unsharded form in anything you write down: it has no shard number to rot.

### Reproducing CI's contention

CI runs the whole `#[ignore]`d suite at once on a 4-vCPU runner, so the contention that produces these
failures comes from dozens of probe JVMs competing, not from CPU scarcity alone. To reproduce that, pin the
whole suite instead of adding load:

```bash
taskset -c 0-3 cargo test --test mcp_integration -- --ignored --nocapture
```

Pass **no** `--test-threads`: `integration-test.sh` computes one (**4x cores, capped at 40** — TEST-32) and
prints it, and under `taskset -c 0-3` that comes out at **16**, which is exactly what CI passes. So the
recipe still reproduces CI's contention; it just is not libtest's default any more. Overriding with
`JDWP_TEST_THREADS`, or an explicit `--test-threads`, changes the shape and stops it being CI's.

And prefer this to CPU hogs — a hog-based arm leaked 32 processes twice, because `trap … EXIT` does not fire
on SIGKILL.
