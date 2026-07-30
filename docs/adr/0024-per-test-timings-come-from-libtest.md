# 0024 — Per-test timings come from libtest's `--report-time`, not cargo-nextest

**Status:** accepted (TEST-26, #103)

## Decision

The integration suite reports per-test durations through libtest's own `--report-time`, reached on a stable
toolchain with `RUSTC_BOOTSTRAP=1` set **on the test binary only**. `scripts/integration-test.sh` therefore
builds with `cargo test --no-run` and then runs the resulting binary directly, and
`scripts/test-timings.py` ranks the durations out of the log.

`cargo-nextest` was the other candidate and was rejected.

## Why not nextest

Nextest is the obvious suggestion — it reports per-test timings natively, has a slow-test threshold, needs
no unstable flags, and is a prebuilt binary CI could install in seconds. Three reasons it costs more here
than it looks.

**It schedules a process per test, so adopting it is a scheduling change.** That is not a side effect to be
managed; it is nextest's execution model. #103 puts a change to test parallelism or scheduling explicitly out
of scope, and the reason is written down in `CLAUDE.md`: how many probe JVMs contend at once is the variable
this repo has already misread twice, in two separate flake investigations that reasoned backwards about
contention. There are four flakes open right now — #45, #56, #64, #71. A measurement change and a
contention change landing together would make both unreadable, and the measurement is what the other three
test-speed issues were waiting on.

**Nextest is not faster here for the reason people assume it is.** Its headline win is process-per-test
isolation and better scheduling of *many short* tests. This suite is 117 tests over 647 s of test time,
already running 3.7x concurrent on four cores against a theoretical 4.0x — so it is within about 8% of
saturating the cores it is given. There is very little scheduling slack for a better scheduler to reclaim.
Treating "nextest is faster" as the reason to adopt it would have been another unmeasured estimate, which is
the specific failure #103 exists to stop.

**The three green-run-of-nothing guards are built on the pretty output.** `scripts/integration-test.sh`
fails a run that printed `SKIP … no JDK found`, a run whose result line says `0 passed`, and a run with no
`JDK in use:` line — and the workflow re-checks the first of those per leg. All three read libtest's stdout
under `--nocapture`. Nextest captures per-test output and reports it in its own shape, so each guard would
have to be re-expressed against a different format, and every one of them exists because a green run of
nothing already happened here at least three times. Rewriting them to buy timings is a bad trade.

None of this says nextest is the wrong tool for this repo forever. It says the change should be judged on
its own, against the flakes being closed and with the timings already in hand — not smuggled in as the
implementation of a measurement issue.

## Why `RUSTC_BOOTSTRAP=1` never touches `cargo`

`--report-time` is gated behind `-Z unstable-options`, which libtest accepts only on a nightly toolchain or
with the documented, unsupported `RUSTC_BOOTSTRAP` bypass. The bypass is acceptable on the **test binary**,
where the only thing that reads it is libtest's argument check. It is not acceptable on `cargo`, and this was
measured rather than assumed:

- Cargo hashes `RUSTC_BOOTSTRAP` into the build fingerprint. Setting it on a `cargo test` invocation
  recompiled the workspace — 14 s locally — and alternating between a run with it and a run without it
  recompiles every time.
- Worse than the cost: the compile would then run under a flag that makes nightly-only features build
  silently on a stable toolchain. A nightly dependency could arrive with nothing saying so, in a repo whose
  toolchain is pinned and checked by its own workflow.

Splitting build from run is what keeps the variable out of the compiler. It also loses nothing: everything
`cargo` gives the tests reaches them through compile-time `env!` — `CARGO_BIN_EXE_jdwp-mcp`,
`CARGO_MANIFEST_DIR` — so those paths are baked into the binary. Verified by running the full suite this
way: 117 passed, the same set as before.

The split is a prerequisite for TEST-27 (#104) besides, which needs the JDK legs to run a **prebuilt** test
binary rather than build their own copy.

## The failure mode, and why it does not gate

A future toolchain could remove the bypass. libtest rejects an unaccepted flag before running any test and
exits 101, so the runner script retries once without the timing flags: the cost is a fraction of a second,
the suite still gates, and the timing report degrades to a note naming the missing variable. Timings are an
instrument, not a gate — the three guards above are the gate.

Both refusal messages are matched, on their shared tail. That detail is here because grepping for one of the
two sentences was wrong and was only caught by defeating the fallback on purpose: `-Z` without the bypass
says "the option `Z` is only accepted on the nightly compiler", while `--report-time` without `-Z` says
"The \"report-time\" flag is only accepted on the nightly compiler with -Z unstable-options". A related
omission in the same change — the run's stderr was not being teed into the log, so a refusal could never
appear in the file the fallback greps — was found the same way.

## Consequences

- Every run prints a ranked slowest list, locally and in all three CI legs' job summaries.
- The suite's two headline numbers are now separable: **test time** (occupancy, 647.4 s) and **wall clock**
  (177.3 s under `taskset -c 0-3`). TEST-29 (#106) needs the ratio between them, and TEST-27 (#104) needs
  neither to include per-leg fixed cost.
- `scripts/integration-test.sh` no longer runs the suite through `cargo test`. Its argument contract to the
  caller is unchanged: arguments still go straight to libtest and the script still supplies the `--`.
