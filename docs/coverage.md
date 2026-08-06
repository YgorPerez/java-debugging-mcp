# Coverage

How coverage is measured here, the gaps that were reviewed once with a verdict each, and **the standing decision
not to gate on a percentage**.

Salvaged out of `TODO.md` when that file was deleted. It is cited from
[`.github/workflows/coverage.yml`](../.github/workflows/coverage.yml) and from ADR-0007, both of which rely on
the no-gate decision at the bottom, and from two test comments that cite specific verdicts in the review.

> **Every figure here is dated evidence, not a current number.** The suite moves weekly and these are a snapshot
> of one commit. `gh run list --workflow=coverage.yml` finds the latest run and its job summary carries the
> table. The review's *verdicts* are the durable part; the percentages are not.

## The measurement

**89.58% region / 89.74% line / 86.14% functions**, unit + integration together — 240 unit + 6 doc + 7 stdio
+ 228 integration tests (217 of them `#[ignore]`d and needing a JVM, 11 cassette tests that do not), zero
skips. Measured in CI on `7a0462e`, 2026-08-05
([run 31020275416](https://github.com/YgorPerez/java-debugging-mcp/actions/runs/31020275416)).

Up from **86.29% / 87.77% / 80.52%** (58 unit + 51 integration) at TRACE-7/TEST-9/CLEAN-1/TEST-8, from
**85.28% / 86.64% / 79.62%** at TEST-7 (#19), and from 83%/78% at TEST-5. **Every column rose while the
suite grew 4.5×**, which is the reading that matters: the growth is not tests chasing a percentage.

Re-measure rather than quote this — the figures above are a snapshot of one commit and the suite moves
weekly. `gh run list --workflow=coverage.yml` finds the latest, and its job summary carries the table.

The move worth naming is not the total: **`main.rs` went from 65.38% region / 66.25% line to 90.07% /
95.88%**, which was TEST-9's whole point — it was the one uncovered path #19 judged a real gap. The new
`mcp-server/tests/stdio_protocol.rs` needs **no JDK**, so unlike everything in `mcp_integration.rs` it runs
in the default `cargo test`.

Measured **in CI**, by `.github/workflows/coverage.yml`, and that is now the only place it can be measured:
`-C instrument-coverage` needs the profiler runtime, and rustup ships no `profiler_builtins` for
`x86_64-pc-windows-gnu`; the msvc toolchain has the runtime but needs the Visual Studio C++ build tools for
`link.exe`. A Windows dev box cannot produce a report at all, which is why the figures sat stale for so
long. `coverage.sh` detects that up front now rather than dying ninety seconds into a build on a bare
`error[E0463]`. Push to `main` or run the workflow by hand; the summary lands in the job summary.

Read it with one caveat in mind: the number is only meaningful because the harness now shuts the server
down by closing stdin rather than `kill()`ing it.
Coverage counters flush in an `atexit` handler, which SIGKILL skips — so before that fix the entire
integration suite contributed **nothing**, and `handlers.rs` measured 3.75% while 35 tests drove it. The
broken instrument produced a plausible-looking low number, not an error.

### Read the function column with care — async breaks it

`handlers.rs` reports **187 of 764 functions "missed"**, which reads as a great deal of dead code and is
mostly an artifact. For an `async fn`, llvm-cov attributes hits to the *generator* the compiler produces,
while the elided outer shell that merely builds the future records **zero**. So `handle_thread_dump` and
`handle_attach` appear as never-executed while 48 tests drive them. Judge a function by the max hit count
across *all* its entries (shell plus closures), not by the shell. The region and line columns are
unaffected.

Worth writing down because it is the same trap in a new costume: a number that reads as a finding and is
an instrument artifact.

### Uncovered paths reviewed, and the verdicts

The question #19 raised — several functions have **no unit test by design** and are covered only through
integration, and "the right seam" and "actually reached" are different claims. Now measured, and the split
is what was intended. Hit counts from the run:

- **The integration-only dump/trace helpers — all genuinely reached.** `describe_caller_chain` 56,
  `collect_dump_rows` 48, `method_name_matches` 45, `read_dump_stack` 29, `read_thread_monitors` 24,
  `monitor_label` 21. **No gap.** These need a live JVM by nature, and the integration tests do reach them.
- **The new `jdwp-client` primitives — all reached.** `capabilities` 70, `owned_monitors` 14,
  `current_contended_monitor` 14, `set_method_exit_request` 3, `clear_method_exit_request` 3,
  `can_get_method_return_values` 2. The error arms of the last three are thin at 2–3 calls; **a real but
  low-value gap**, since each is a JDWP failure path that a healthy `HotSpot` will not produce on demand.
- **`resume_all_fully`'s exhaustion tail** (`thread.rs`) — the function itself is now the most-exercised
  path in the client (**91** hits), but the branch reporting "the VM is STILL suspended" after
  `MAX_RESUME_ATTEMPTS` is **still unreached**, and still **a deliberate gap**: reaching it needs a suspend
  depth above 8, and with `debug.pause` idempotent (ADR-0003) no sequence of *this tool's own calls* can
  build one. **Unreachable through the tool's own API** — only something outside the session suspending
  concurrently gets there, which is [#13](https://github.com/YgorPerez/java-debugging-mcp/issues/13)
  territory. The honest-failure path of the safety fix remains the untested one.
- **`get_thread_status`** (`thread.rs`) — **closed, confirmed by measurement.** 39 hits. TEST-5 recorded
  it as covered-by-prediction once DUMP-1 landed; the prediction was right.
- **`Value::format`** (`types.rs`) — **51 hits, not dead code**, confirming the earlier verdict. Note
  `types.rs` still shows 16.67% region: the file is one big match over value kinds and most arms are for
  types the probes never produce. Low percentage, not a finding.
- **`get_id_sizes`** (`vm.rs`) — **0 hits, genuinely never executed**, the only named function in this
  review that was. **Deleted** by CLEAN-1 (#27): nothing called it and nothing needed to, since the reader
  assumes 8-byte ids outright. The one caller that existed was `examples/test_vm_commands.rs`, an ad-hoc
  manual harness nothing runs — which is why the coverage run measured zero, and is not a use. The
  assumption it nominally guarded is now stated where the reader actually makes it (the header of
  `reader.rs`), because an uncalled `IDSizes` wrapper made the widths look *checked* when they are not.
- **`get_version`** (`vm.rs`) — **now reached** (2 hits, via attach). TEST-5 paired the two as
  "conveniences the server never calls", and that verdict has now expired in both directions: this one is
  reached, and the other one is gone. Recorded because a stale verdict is worse than none.
- **`main.rs` at 65.38%** — the stdio read loop and its malformed-message arms. **A real gap**, and the one
  taken next: closed by TEST-9 (#25) with `mcp-server/tests/stdio_protocol.rs`, seven tests that need no
  JDK. It found a hang, not a percentage — see the shipped entry below.

There is deliberately **no coverage percentage gate** — the standing decision, and the reason is that a
percentage rises with tests that assert nothing. The value is the list above — and it paid for itself on
the first run, which found a broken instrument rather than a low number.


## Why there is no percentage gate

Stated once more here because it is the part other files cite as a standing decision rather than as history: **a
percentage rises with tests that assert nothing**, so gating on one buys a number and not a property. The value
is the reviewed list above — a verdict per uncovered path, with "deliberate gap", "real but low-value" and "real
gap, taken next" as distinct answers — and it paid for itself on its first run by finding a broken instrument
rather than a low number.

ADR-0007 draws the deliberate contrast: `doctor` **is** the gate and fails on warnings, while coverage is
measured and read. The two are different kinds of check and only one of them is allowed to block.
