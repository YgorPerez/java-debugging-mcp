# java-debugging-mcp — improvement backlog

Tracked as independently-grabbable vertical slices (per the `/to-issues` convention): each item is a
complete end-to-end capability — JDWP primitive(s) in `jdwp-client` + wiring/tool in `mcp-server` +
a validation against a live probe — not a horizontal layer. A fresh session can grab any unblocked
item and finish it.

## Coverage: `scripts/coverage.sh`, and the gaps reviewed once

**86.29% region / 87.77% line / 80.52% functions**, unit + integration together — 58 unit + 6 doc + 7 stdio
+ 51 integration tests, zero skips (TRACE-7/TEST-9/CLEAN-1/TEST-8). Up from **85.28% / 86.64% / 79.62%** at
TEST-7 (#19), and from 83%/78% at TEST-5.

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
- **`resume_all_fully`'s exhaustion tail** (`thread.rs:223`) — the function itself is now the most-exercised
  path in the client (**91** hits), but the branch reporting "the VM is STILL suspended" after
  `MAX_RESUME_ATTEMPTS` is **still unreached**, and still **a deliberate gap**: reaching it needs a suspend
  depth above 8, and with `debug.pause` idempotent (ADR-0003) no sequence of *this tool's own calls* can
  build one. **Unreachable through the tool's own API** — only something outside the session suspending
  concurrently gets there, which is [#13](https://github.com/YgorPerez/java-debugging-mcp/issues/13)
  territory. The honest-failure path of the safety fix remains the untested one.
- **`get_thread_status`** (`thread.rs:117`) — **closed, confirmed by measurement.** 39 hits. TEST-5 recorded
  it as covered-by-prediction once DUMP-1 landed; the prediction was right.
- **`Value::format`** (`types.rs:102`) — **51 hits, not dead code**, confirming the earlier verdict. Note
  `types.rs` still shows 16.67% region: the file is one big match over value kinds and most arms are for
  types the probes never produce. Low percentage, not a finding.
- **`get_id_sizes`** (`vm.rs:76`) — **0 hits, genuinely never executed**, the only named function in this
  review that was. **Deleted** by CLEAN-1 (#27): nothing called it and nothing needed to, since the reader
  assumes 8-byte ids outright. The one caller that existed was `examples/test_vm_commands.rs`, an ad-hoc
  manual harness nothing runs — which is why the coverage run measured zero, and is not a use. The
  assumption it nominally guarded is now stated where the reader actually makes it (the header of
  `reader.rs`), because an uncalled `IDSizes` wrapper made the widths look *checked* when they are not.
- **`get_version`** (`vm.rs:47`) — **now reached** (2 hits, via attach). TEST-5 paired the two as
  "conveniences the server never calls", and that verdict has now expired in both directions: this one is
  reached, and the other one is gone. Recorded because a stale verdict is worse than none.
- **`main.rs` at 65.38%** — the stdio read loop and its malformed-message arms. **A real gap**, and the one
  taken next: closed by TEST-9 (#25) with `mcp-server/tests/stdio_protocol.rs`, seven tests that need no
  JDK. It found a hang, not a percentage — see the shipped entry below.

There is deliberately **no coverage percentage gate** — the standing decision, and the reason is that a
percentage rises with tests that assert nothing. The value is the list above — and it paid for itself on
the first run, which found a broken instrument rather than a low number.

## Settled decisions live in `docs/adr/`

Why read-only is enforced at the wire boundary, why the trace budget isn't JDWP's `Count`, why suspends have
to be counted, why an auto-disarm disables rather than deletes, why stop-point ids aren't request ids, why
expansion is opt-in, why `doctor.sh` is the lint gate, and why a traced stop point times its capture window
and nothing else — each with the rejected alternative and the evidence. See [`docs/adr/`](docs/adr/README.md). The two sections below are the operational summaries; the
ADRs are the reasoning.

## `cargo clippy` does not lint the integration tests — run `scripts/doctor.sh`

The lint policy lives as `#![warn(clippy::pedantic, …)]` crate attributes in `jdwp-client/src/lib.rs`
and `mcp-server/src/main.rs` (see the note in `Cargo.toml`). Those apply to **those crates**.
`mcp-server/tests/mcp_integration.rs` is a *separate* crate with no such attributes, so
`cargo clippy --workspace --all-targets` reports **zero** warnings on it however bad it is.

rust-doctor passes the lint flags on the command line instead, so it *does* cover the test crate. That
difference hid nine real warnings in test code that had been reported as "clippy clean" across several
commits (`i64 as usize` casts, redundant clones, missing doc backticks).

**So: `cargo clippy` is not the gate. `scripts/doctor.sh` is.** Run it before claiming a change is clean,
and `scripts/doctor.sh --diff main` to see only what you changed.

**And `scripts/doctor.sh --findings` to see *what* it found, not how many** (LINT-3, #42). The summary box
gives a count and no way to reach the findings behind it — on a run reporting five warnings, grepping the
output for `⚠`, for `warning`, for the rule name and for `threshold` each returned nothing. That is what
cost the v0.2.0 release: the tag build failed this gate on five `excessive-clone` findings that were all
sitting in a local run beforehand, and the count going 1 → 5 was dismissed because there was no cheap way
to see what the five were. `--findings` prints each warning/error in the same shape CI's step summary uses,
says whether `--fail-on warning` would pass, exits 3 if it would not, and names what the run did **not**
look at (passes skipped for a missing tool, and passes that ran only because you have a tool CI does not
install). **The score is not the gate**: 100/100 "Great" has been observed on a scan carrying 21 warnings.

**The gate fails on warnings, on a pinned toolchain** (LINT-1, #18). It used to gate on errors only, and
the zero-warning state reached in `7253499` drifted back to seven over twenty-four commits — then got
described as pre-existing debt rather than as the regression it was. Warnings are back to **0**, and CI
now enforces it (`--fail-on warning`, pinned to Rust 1.97.1 so a new upstream lint becomes a scheduled
bump rather than a broken build on code nobody touched). The reasoning and the rejected options are in
[ADR-0007](docs/adr/0007-doctor-not-clippy-is-the-lint-gate.md).

The five findings that had drifted back in were all mechanical, and were fixed the way the previous batch
fixed its own: by extraction. `render_dump_header` → `dump_filter_note` + `dump_monitor_caveats`;
`handle_set_field_stop` → `watch_kinds` + `arm_one_field_watch` + `render_field_stop_reply`;
`handle_set_exception_stop` → `render_exception_stop_reply`; `handle_get_stack` → `render_stack_frame`
over a `StackWalk`/`StackWalkState` pair; `handle_set_value` → `set_field_by_path`. The three
`.clone()`-in-a-loop findings were restructured rather than relocated — the exhausted-local name is now
copied once on the way out instead of per frame, and a filtered map's surviving keys are moved out of the
scan rather than cloned out of a shared vector per match.

One finding is known and deliberately allowed, and it is a *dependency* issue rather than a code one:

- **`multiple versions for dependency syn`** — `schemars` pulls `syn 3`, `serde_derive` pulls `syn 2`.
  Not fixable without dropping one of them; costs build time, not correctness. Now declared as
  `allowed-duplicate-crates = ["syn"]` rather than tolerated, so the *next* duplicate — which would be a
  real finding — still fails the build instead of hiding behind this one. It was previously recorded here
  as scoring "info, so the warning gate does not trip on it", which was simply **wrong**: it is a
  warning, once per crate, and it failed the first gated CI run.
  The allowance lives in `jdwp-client/clippy.toml` and `mcp-server/clippy.toml`, **per crate rather than
  at the workspace root**, and that took two attempts. A root `clippy.toml` satisfies `cargo clippy`,
  which walks up from `CARGO_MANIFEST_DIR` — it went clean locally — but not rust-doctor's invocation,
  which is the gate that counts. CI named the path it looked in and did not find: `<crate>/clippy.toml:1`.
  Keep the two files in sync.

**On `windows-gnu`, `scripts/doctor.sh` cannot verify the warning count — don't trust a local 0.** Its
isolated `target/rust-doctor` build fails to link (`ld.exe: cannot find \symbols.o`), and a build that
cannot link is a **clippy pass that cannot run**. So a Windows doctor run reports only the custom AST
rules and contributes zero clippy findings — it says 0 because it did not look. That is how LINT-1 was
verified clean locally and then failed CI on three clippy findings, one of them a `doc_markdown` in the
integration-test crate, which is precisely the blind spot ADR-0007 exists to describe.

Locally on Windows, get the clippy half from `cargo clippy` with doctor's own flags — that path uses the
normal target dir and works fine:

```
cargo clippy --workspace --all-targets -- -W clippy::pedantic -W clippy::nursery -W clippy::cargo
```

`--all-targets` is what reaches `tests/mcp_integration.rs`, and `-W clippy::cargo` is what surfaces
`multiple_crate_versions`. Neither is on by default, which is why plain `cargo clippy` looked clean the
whole time. Then run `scripts/doctor.sh` for the custom rules.

**And on any platform, `scripts/doctor.sh` on the wrong toolchain cannot verify it either.** The gate is
pinned to **1.97.1** (LINT-1's whole point: a new lint should be a scheduled bump, not a surprise
build break). The cost of that pin is the mirror image: an **older** local toolchain does not have the
newer lints, so it reports 0 for code the gate fails on. That cost a red `main` — two
`Duration::from_millis(1000)` in test code passed a local 1.94 run at 100/100 and failed CI on
`clippy::duration_suboptimal_units`, added in 1.97. `doctor.sh` now compares the active rustc against the
pinned one (read out of the workflow, so they cannot drift) and says so loudly rather than letting a clean
run be believed:

```
rustup toolchain install 1.97.1 --component clippy
RUSTUP_TOOLCHAIN=1.97.1 scripts/doctor.sh --fail-on warning
```

Same family as the rest: SIGKILL'd coverage counters, an undetectable JDK, a filter matching no tests, a
warm cache linting only what it rebuilt. **Five** ways to get a green run that examined nothing, and every
one of them was found by something other than the check itself.

## The resume-honesty invariant (read this before touching a resume path)

Five reviews in, **every round's most serious bug was in the previous round's safety work**, and the
watchdog was wrong three times (SAFE-2 → SAFE-5 → SAFE-7). The shape never varied: a resume path was
tested in the one state its author had in mind and broke in a state nobody enumerated.

So there is now a test for the invariant itself, not another happy path
(`mcp_integration.rs`, `*_is_honest_from_every_suspended_state`):

> After **any** resume path, from **any** suspended state, the VM is genuinely running — or the reply
> said out loud that it isn't.

It is a matrix of 5 suspended states × 4 resume paths (`continue`, `panic`, watchdog, `disconnect`),
asserted against the **probe's own output**, because every tool reports success either way — which is
exactly how these bugs survived. Each of SAFE-1, SAFE-4 and SAFE-7 was reverted in turn to confirm the
matrix names the offending `(state, path)` pair rather than passing anyway.

**If you add a resume path, add it to `Resume`. If you find a new way to leave the VM suspended, add it to
`Freeze`.** That is cheaper than the next review finding it, and it is the whole point of the matrix.

Its scope is deliberately stated in the test: it covers *resume* honesty, not *disarm* honesty (a VM that
resumes but is immediately re-frozen by a still-armed stop point — the SAFE-2/SAFE-5 harm). That half is
covered by two tests that measure the probe's tick **rate** after a rescue. Folding them together needs a
repeating-breakpoint state whose expectation differs per path, since `continue` may legitimately re-freeze
and a rescue path may not.

## How to validate anything here (the house pattern)

Two layers, two mechanisms. Pick by what the change touches.

**MCP-server behaviour (handlers, event pump, session state) — an integration test.**
These drive the real `jdwp-mcp` binary over JSON-RPC on stdio against a real probe JVM, and they run
from `cargo test`:

```
scripts/integration-test.sh              # all of them
scripts/integration-test.sh force_return # filter by test name
```

Add cases to `mcp-server/tests/mcp_integration.rs`; the harness (`tests/common/mod.rs`) compiles and
launches the probe, picks a free port, reaps the JVM, and captures the probe's stdout+stderr. Write a
probe under `examples/probes/` and mark breakpoint lines with `// BP<n>` comments — tests locate them
by marker, so editing the Java can't silently point a test at the wrong statement.

They are `#[ignore]`d (they spawn JVMs and need a JDK), which is why the runner passes `--ignored`.
**With no JDK every test prints `SKIP` and passes** — so a green run on a JDK-less machine proves
nothing; grep the output for `SKIP`.

**Raw `jdwp-client` protocol work — an example.**
Register an `examples/test_*.rs` in `jdwp-client/Cargo.toml` and run it against a hand-launched probe:

1. Compile the probe with `-g` (without it there is no local-variable table, so no locals can be
   read at all). No system JDK on this box — the JBR at `/snap/intellij-idea-ultimate/*/jbr/bin/javac`
   is the only one, which is also why the test harness looks there.
2. `java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:<port> -cp . Probe`
   (dt_socket `server=y` accepts ONE connection then stops listening — fresh port per run).
3. `cargo run --release --example …`

Worked patterns: `examples/test_static_field.rs`, `examples/test_deferred_bp.rs`.

**Shared-instance behaviour — a shaped probe plus the latency relay. You do not need the 8180.**
This was the standing excuse for leaving the shared-instance defaults uncalibrated, and it does not hold.
What makes a real app server different from a loopback probe is three variables, and two of them belong to
the *debuggee*:

| variable | how to present it here |
| --- | --- |
| hundreds of threads, not 60 | `PoolShapeProbe` — `WORKERS = 300`, named like a real pool's |
| stacks far deeper than 8 frames | `PoolShapeProbe` — `DEPTH = 60`, **distinct** methods, not recursion |
| threads in DIFFERENT code, not one shape | `MixedPoolProbe` — 300 workers across 10 handlers over a shared 40-frame framework prefix |
| a network hop instead of loopback | `LatencyRelay::start(probe.port, Duration::from_millis(4))`, then attach to `relay.port` |

`LatencyRelay` (in `tests/common/mod.rs`) forwards the JDWP stream with a delay per chunk. Userspace
because `tc qdisc … netem delay` needs `NET_ADMIN`, which a container lacks — and because deterministic
latency is better for a test than a real network's jitter. It charges coalesced traffic once, so a
measurement through it is a **lower bound**; it models latency only, not loss or bandwidth.

Depth must be **distinct methods**. Line tables are cached per dump by `(class, method)`, so a recursive
chain collapses to one lookup and flatters the cache; a real request stack has about as many methods as
frames (ADR-0011).

Prefer asserting **packet counts** over durations for anything cost-related: a packet count is
deterministic and independent of machine load, a duration is neither. See
`a_production_shaped_dump_costs_a_bounded_number_of_packets_per_thread`, whose ≤20-per-thread bound fails
at ~70 if the cache is removed — verified by defeating it, not assumed.

What still needs the real instance is only its **own parameters**, and a dump now reports them: one
`debug.thread_dump` at defaults gives the thread count in its header and the per-packet cost (hence the RTT)
on its cost line, and a truncation states what finishing would have taken. No `ping`, no arithmetic. The
figures below are what to compare that reading against.

Note: the `mcp__jdwp__` tools in a running Claude Code session hold the OLD binary — a rebuild is only
picked up after a Claude Code restart. That's why validation goes through tests/examples, not the live
tools, within a session. The integration tests sidestep this entirely: they spawn the binary Cargo just
built for that run (`CARGO_BIN_EXE_jdwp-mcp`), so they can never test a stale binary.

---

## ✅ Shipped (context)

- **Static-field reads in `debug.evaluate`** — `ConfigDefaultUtils.dsUrlMotor`, with or without a
  suspended frame. Primitives `get_reference_values` (ReferenceType.GetValues) + `all_classes`.
  Validated (`examples/test_static_field.rs`).
- **Deferred / class-prepare breakpoints** — a breakpoint on a not-yet-loaded class auto-arms on
  load. Primitives: ClassPrepare wire-decode, `set_class_prepare`/`clear_class_prepare`,
  `resume_thread`. Validated end-to-end (`examples/test_deferred_bp.rs`).
- **`debug.force_return`** — force the current method to return a value, skipping its body.
  Primitive `force_early_return` (ThreadReference.ForceEarlyReturn). Now runtime-verified: the test
  forces boolean/String/int returns and reads the probe's OWN stdout to confirm the caller received
  the forced value, not just that the debugger reported success.
- **Exception breakpoints (EXC-1)** — `debug.set_exception_stop {class_pattern, caught,
  uncaught}` breaks on a thrown exception (type + subclasses), reported via `debug.get_last_event`
  with the exception type + caught/uncaught + catch location. Primitives: `Exception` wire-decode,
  `set_exception_request`/`clear_exception_request`. `list`/`clear`/`panic` are aware. Validated
  end-to-end — target-only filter, non-target never fires (`examples/test_exception_bp.rs`).
- **Live field writes, static + instance (SETF-1)** — `debug.set_value {target, value}` now writes
  a local, a static field (`ConfigDefaultUtils.dsInfra`), or an instance field (`this.status`),
  with type coercion + declared-type validation. Primitives `set_reference_values`
  (ClassType.SetValues) + `set_object_values` (ObjectReference.SetValues). Legacy `name` key still
  accepted. Validated — static String/int + instance int/String/boolean written and read back
  (`examples/test_set_field.rs`).
- **`jdwp-trace` skill / swallowed-exception playbook (DOC-1)** — lives in the sibling
  `infotravel-dev-toolkit` repo (`skills/jdwp-trace/`, commit `8546bbf`), cross-linked from
  `ask-infotravel` and the README, with a pointer from `run-infotravel` § 3 left in that repo's working
  tree (its SKILL.md had unrelated uncommitted edits, so the hunk wasn't swept into this commit).
  Three silent-catch sites, each **verified against the current Java source** rather than transcribed —
  and the first one differs from this backlog item's description: in `IntegraSrv.post` the swallow is a
  missing `else` on `if (status == 200)` (any non-200 returns `null`, status recorded nowhere), not a
  silent catch; that catch does log, but rethrows `ErrorException(result)` with `result` null, and
  `ErrorException(String)` has no cause. `EnviaEmailSrv.enviarEmailIntegrador:162` is the complete
  swallow (its `save()` is commented out, so the failure it records dies with the method).
  Recipes are built on `trace:true` logpoints, since 8180 is shared and a normal breakpoint freezes the
  whole VM — which is what made TRACE-1 the prerequisite.
- **Collection search/filter (OBJ-2)** — `[…]` subscripts in `debug.evaluate` expressions:
  `lines[0]` / `counts["key"]` (index — narrows to one value, so `lines[0].sku` keeps chaining),
  `lines[2..5]` (half-open slice, clamped rather than erroring on an over-long range), and
  `lines[?qty > 3]` (predicate filter). In a filter the **left side resolves against each element**
  (`[?status == "OPEN"]`, `[?getQty() == 2]`) so there is no element variable to learn; the right side
  is a literal or an ordinary expression read from the frame (`[?qty > order.threshold]`).
  A `Map` key that is a primitive is boxed via `Wrapper.valueOf` before `get(Object)` sees it.
  Slices and filters select several values, which JDWP has no value type for, so they **end the
  expression** — `Resolved::Many` is terminal and chaining after one is refused with an explanation
  rather than silently picking the first. Results report `N of M matched`, so "0 matched" is
  distinguishable from "nothing was scanned", and a predicate that fails on *every* element is an
  error rather than an empty result. Scanning is capped (1000 elements) and says when it truncated.
  Validated by `collection_subscripts_…` in `mcp_integration.rs`.
  Two bugs this surfaced, both fixed: (1) JDWP **invalidates a thread's frame ids** as soon as a
  method is invoked on it, so `toArray()` staled the frame before a right-hand side like
  `order.threshold` could be read — the predicate's element-independent half is now resolved *before*
  scanning, which also stops it being re-evaluated per element; (2) `debug.set_value` parsed a
  subscript in its target and then dropped it, writing the whole field instead of the named element —
  now refused explicitly. Shallow rendering also unboxes wrappers now, so `counts["b"]` reads
  `(int) 2` rather than `java.lang.Integer "2"`, matching the deep renderer.
- **Recursive object expansion (OBJ-1)** — `expand_objects:true` on `debug.evaluate` /
  `debug.get_stack` renders a value as an indented field tree instead of one line: nested objects
  (own + inherited fields), array elements, and element-level `List`/`Set`/`Map`/`Optional` contents.
  Bounded by `max_depth` (default 2), `max_children` (16) and a total node budget (400) that reports
  when it is hit; **cycle detection** is path-based, so `customer.self` and a parent↔child
  `order.customer.lastOrder` render as `↩ Type (id=…, cycle)` instead of recursing. Boxed wrappers
  unbox, so a `List<Integer>` is twenty numbers rather than twenty `Integer { value = … }` blocks. At
  the depth limit it falls back to the shallow `toString()` summary, which is the most informative
  single line available. Off by default: expansion invokes `toArray`/`entrySet`/`getKey` in the
  debuggee, which needs a suspended thread and has side effects, so the cheap path stays cheap.
  Container detection is duck typing on `size()`+`toArray()`/`entrySet()` rather than an interface
  check — `ReferenceType.Interfaces` returns only *direct* superinterfaces, so a real
  `instanceof Map` test costs many round trips per rendered object; a false positive only renders
  something element-wise that isn't a collection, which is odd but never unsafe.
  Validated by `deep_expansion_…` in `mcp_integration.rs` against `examples/probes/DeepProbe.java`,
  which carries every shape on purpose: cycles, an inherited field, an over-cap list, empty
  collections, an empty `Optional`, and both primitive and object arrays.
- **MCP-handler integration tests (TEST-1)** — `mcp-server/tests/mcp_integration.rs` +
  `tests/common/mod.rs`: the real `jdwp-mcp` binary driven over JSON-RPC on stdio against real probe
  JVMs, runnable with `scripts/integration-test.sh` (no manual steps — the harness compiles the probe,
  picks a free port, launches and reaps the JVM, and captures its stdout+stderr). Four tests cover the
  expression handlers, watchpoints, the **deferred-breakpoint** `CLASS_PREPARE` path, and
  **force_return**. The `Server` helper the two ex-example harnesses each carried a copy of now lives
  in one place, and those examples are deleted. Tests spawn `CARGO_BIN_EXE_jdwp-mcp`, so they can never
  run against a stale binary. With no JDK they print `SKIP` and pass, keeping a JDK-less CI green.
  Gotcha found: `Class.getMethod` only sees *public* methods, so reflecting into a package-private
  probe method throws `NoSuchMethodException` — and because the harness was discarding the probe's
  stderr, that surfaced as a bare timeout. The harness now folds stderr into the captured output.
- **Field watchpoints (WATCH-1)** — `debug.set_field_stop {class_name, field_name, modify, access}`
  breaks when a field is written (or read), answering "who mutates this?". A write hit reports the
  mutating `class.method:line`, the field, whether it's static, the owning instance, and the
  **old → new** pair — the old value is read while the thread sits suspended *before* the pending
  store commits. A read hit reports a single `value` instead. Primitives: `FIELD_ACCESS` /
  `FIELD_MODIFICATION` wire-decode (incl. JDWP's `valueToBe`), `set_field_watch`/`clear_field_watch`
  with a `FieldOnly` modifier, and a `WatchKind` enum. `list`/`clear`/`panic` are aware (a watch
  survives `ClearAllBreakpoints`, so panic clears them explicitly). Validated end-to-end through the
  real MCP server — 17 checks (`examples/test_watchpoint.rs` + `examples/probes/WatchProbe.java`),
  covering static + instance modify, access-only, and the bookkeeping/error paths.
  Gotchas found: JDWP's `FieldOnly` modifier is modKind **9** (`ClassOnly` is 4) — the wrong number
  returns a bare `INTERNAL` (113), naming nothing, so the modifier kinds are now named constants.
  And `static final int` is inlined by javac, so a `FIELD_ACCESS` watch on one never fires.
- **Static-method invocation + object arguments (EVAL-1, EVAL-2)** — `debug.evaluate` now calls
  static methods off a class head (`EvalProbe.twice(21)`, `ConfigDefaultUtils.getUrl()`) and accepts
  **expressions** as arguments, passed by reference (`a.matches(b)`, `EvalProbe.describe(a)`,
  `EvalProbe.twice(a.plus(1))`). Primitive `invoke_static_method` (ClassType.InvokeMethod);
  overloads are resolved against each argument's **runtime class chain**, so `pick(Item)` beats
  `pick(Object)` where a tag-only match couldn't tell them apart. Breakpoint conditions inherited the
  same generalization — the right-hand side may now be an expression (`total > Config.LIMITE`,
  `a.name == b.name`), compared value-to-value (Strings by content, other objects by identity).
  Validated end-to-end through the real MCP server over JSON-RPC (`examples/test_eval_invoke.rs` +
  `examples/probes/EvalProbe.java`), 19 checks.
  ⚠️ **Safety note discovered here**: the old arity-only overload fallback could hand the JVM an
  `int` for a reference parameter, which reads it as an oop and **SIGSEGVs the debuggee** (reproduced
  — hs_err + core dump). The fallback is now kind-checked, so a type-mismatched invoke is refused
  with an error instead of crashing the target.
- **Verified against a real Spring Boot app (TEST-3)** — the roadmap criteria were re-run against a live
  **Spring Boot 2.6 + `micrometer-registry-prometheus`** app with 84 real meters, not the stand-in.
  Substituted `golv2` (a real infotravel service, already built as a fat jar on this box) for the
  companion `java-example-for-k8s` the item named, which still isn't here — the point of the item was a
  genuine Spring app, and this is a better one: it's the actual stack this debugger exists for.
  Everything held. `MetricsEndpoint.listNames` broke by **method name** with no line number,
  `this.registry.getMeters().size()` read `(int) 84`, `getMeters()[0].getId().getName()` chained through
  index + calls to a real string, a predicate filter over 84 meters answered `8 of 84 matched`, and a
  `Map`-entry filter (OBJ-4, shipped the same day) answered `5 of 84 entr(ies)` with keys intact.
  The differences are all about **finding**, as the item predicted: the registry is `MetricsEndpoint.registry`
  (not a controller's `meterRegistry`), and — the one that matters — real Micrometer keys `meterMap` by a
  `Meter.Id` **object**, so the stand-in's `meters["name"]` has no real equivalent (`meterMap["logback.events"]`
  correctly returns `null`). On the real thing you filter, which also handles the fact that one metric
  *name* is many meters, one per tag combination — usually why a metric looks "missing".
  **One defect fixed as a result**: filtered map keys are now rendered with `toString()`. They had read as
  `Meter$Id (id=0xaf)` — accurate and useless — when the key is the identity you just filtered for. A
  filter already invokes a predicate per value, so rendering the surviving keys adds no new side effects.
  Recorded in `examples/observability-debugging.md` § "Against a real Spring Boot app". Deliberately **not**
  a checked-in test: it needs a 50 MB prebuilt jar from a sibling repo that no CI runner has. The stand-in
  test stays the automated guard; this is the human-verified evidence behind it.
- **Subscript writes and `Map`-entry filtering (OBJ-4)** — the two gaps OBJ-2 left, both closed.
  **Writing through a subscript**: `set_value {target:"numbers[0]"}` now works on an array (new
  `ArrayReference.SetValues` primitive — no invocation, so no side effects), a `List` (`set(index, value)`)
  and a `Map` (`put(key, value)`). `set` is looked up before `put`, which is unambiguous because a `List`
  has no `put` and a `Map` no `set` — the same arity trick `apply_index` uses for `get`. Both collection
  calls hand back the element they displaced, so the confirmation reports **old → new** without a second
  read. An array write coerces the literal to the array's *component* type, because `SetValues` is
  untagged and a wrong width would corrupt the element silently; a collection write boxes a primitive
  into the wrapper the collection actually holds (the `coerce_args` path added for EVAL-3). A slice or
  filter target is still refused — it names several elements, so there is nothing single to write.
  **Filtering a `Map`**: `byId[?qty > 3]` tests each **value** (the natural reading, and what
  `values()[?…]` already did) but keeps the keys, rendering survivors as `key → value` —
  `Resolved::Many` carries a parallel list of rendered keys, so nothing else in the resolution chain
  changed. Slicing a map is still refused, now naming the filter as the alternative.
  Validated by `subscript_writes_and_map_entry_filters`: each write read back, **neighbouring array
  elements proven untouched**, out-of-bounds and type-mismatched writes refused, slice/filter targets
  refused, and a map filter's keys checked along with a `0 of 5` result and the slice refusal.
  One expectation of mine was wrong first time round, the same way as during OBJ-2: `qty > 3` matches
  three of the five lines, not two.
- **Interface-typed parameters and autoboxing in overload resolution (EVAL-3)** — `arg_type` walked only
  the superclass chain, so a parameter typed as an interface the argument implements (`handle(Runnable)`)
  could never match precisely, and neither could a boxed primitive (`f(Integer)` given an `int`). Both
  fell through to an arity-and-kind fallback that picked the first candidate — right often enough to be
  untrustworthy.
  Overload selection is now two passes, cheap first: plain scoring (no round trips) as before, and only
  if *nothing* scores are the arity-matching leftovers put to the JVM — `ReferenceType.Interfaces`
  (command set 2, command 10), walked transitively through the type cache, plus autoboxing and array
  covariance. An argument that doesn't satisfy a parameter is now **refused**, and a chosen overload's
  reference parameters get their primitives boxed for real via `valueOf` before the invoke.
  ⚠️ **The JVM does not catch this for you.** Measured with the old fallback restored:
  `takesRunnable(anItem)` *succeeded* and returned normally — `InvokeMethod` accepted an object that does
  not implement the parameter's interface. It only looked harmless because that method body ignores its
  argument; one calling `r.run()` would have been operating on a value of the wrong type. Being wrong here
  is silent, which is why the check is strict rather than best-effort.
  Validated in `evaluate_static_methods_and_object_arguments` against new `EvalProbe` overloads: a
  directly-implemented interface, one **inherited through a superclass** (which a direct-superinterface
  query misses), an interface on a JDK type, an `int` boxed into `f(Integer)`, `String[]` into
  `f(Object[])`, and two negative cases that must be refused. The negatives were confirmed load-bearing by
  restoring the old fallback and watching them fail.
- **Container-kind caching: measured, no gain, dropped (PERF-1)** — closed the way the item asked to be
  closed. `classify_container` runs per rendered object and its verdict is a pure function of the type,
  so memoising it looks obviously right. A/B on the heaviest case available (`batch`: 20 identical
  `Order`s × 6 collections each, ~160 classifications) gave **1165 packets either way** and 152–158 ms
  either way — indistinguishable, because those lookups already hit the cached method lists, so the type
  cache had eaten this cost already. The memo was **reverted**: a cache that saves nothing still has to
  be understood, and this one held type ids whose lifetime needed reasoning about. Numbers and reasoning
  are in `docs/VARIABLE_INSPECTION_PLAN.md`, and `ContainerKind` carries a comment so it isn't
  re-proposed.
  Kept from the attempt, because the plan doc promised a measurement method nobody could actually repeat:
  `JdwpConnection::packets_sent()`, a per-session packet count in `debug.list_sessions`, and
  `deep_expansion_stays_within_its_packet_budget` — which records the cost (211 cold / 159 warm) and
  guards against a return to per-object refetching (which measured 421).
- **`debug.list_sessions` (SESS-1)** — concurrent sessions worked but were unfindable: `attach` handed
  back a `session_id` and every tool accepted one, yet a caller who lost it could only reach "current".
  The new tool lists each session's `host:port`, marks the current one, and reports whether it is
  running / SUSPENDED / **DEAD**, with its stop-point count and any buffered traces/events. Liveness is
  read from the event pump — it exits when the connection closes, so a finished task means the JVM is
  gone. Deliberately *not* a JDWP round trip, which could hang on exactly the half-dead socket this is
  meant to diagnose. `DebugSession` now remembers its endpoint (the connection doesn't).
  A dead session is reported, not reaped: this is the tool you reach for when you are already unsure
  what is attached, and one that silently dropped entries mid-listing would be a worse instrument.
  `debug.disconnect {session_id}` is the escape hatch, and the listing says so.
  Validated by `list_sessions_names_every_attachment_and_flags_a_dead_one` — two probes attached, counts
  proven per-session, the older probe killed, the dead one flagged while the live one is untouched.
- **Wire-reader unit tests, and the panic they found (TEST-2)** — the readers had zero coverage against
  malformed input, which a real JVM never produces and a real *bug* does. 12 table-driven tests now
  cover them (`reader.rs`, `events.rs`; plain `cargo test`, no JDK): every `ValueData` variant
  round-trips `write_untagged_value` → `read_value_by_tag`, **every** truncation of every value and of a
  whole event packet returns `Err`, an unknown value tag is refused without advancing the buffer, an
  unhandled event kind degrades to `Unknown`, and `read_string` handles empty / truncated / lying
  lengths and invalid UTF-8.
  What they found: `read_value_by_tag` existed in **three copies** (`eval`, `stackframe`, `object`),
  none of which checked the buffer — `bytes::Buf::get_*` **panics** on a short read (verified against
  `bytes`, not assumed), and `parse_field_modification_event` calls that path with a `valueToBe` straight
  off the wire. A truncated FIELD_MODIFICATION reply would therefore panic the **event-loop task**,
  killing the session rather than surfacing an error. There is now one implementation in `reader.rs`,
  made total by resolving each tag's width up front — so an unknown tag fails before the buffer is
  touched, and a known one is bounds-checked once. `read_string`'s length prefix is checked against what
  is actually left, and every fixed-width reader shares one `ensure` that names the field that ran out.
- **jdwp-client's doc examples compile (DOC-2)** — six illustrative examples were ```` ```ignore ````
  and would not have compiled, so `cargo test -- --ignored` handed `--ignored` to the doctest harness
  and failed for reasons unrelated to whatever you were running — which is the only reason
  `scripts/integration-test.sh` had to scope itself to `--test mcp_integration`. Each is now `no_run`
  with its setup on hidden `#` lines, so the example text stays as short as it was but is **type-checked
  against the real signatures** and can't drift. That caught one already-wrong snippet: the
  `EventLoopHandle` example called the async `send_command` without awaiting it. The scoping workaround
  and its three explanatory comments are gone; the script keeps `--test mcp_integration` only to narrow
  the output.
- **Non-suspending exception breakpoints and watchpoints (TRACE-2)** — `trace:true` now works on
  **all three** kinds of stop point, not just line breakpoints:
  `debug.set_exception_stop {…, trace:true}` and `debug.set_field_stop {…, trace:true}` arm with
  `EventThread`, snapshot the hit, and resume that thread — so the two tools you most want on the
  shared 8180 (silent catches; "who mutates this?") no longer freeze other people's requests. A traced
  throw records the exception type + caught/catch location; a traced write records the **old → new**
  pair, captured at hit time because the old value is only readable before the pending store commits.
  The hit path is one `find_traced_request` lookup across `breakpoints` / `exception_requests` /
  `watchpoints` — deliberately three small scans rather than a fourth map keyed by request id, which
  would be a second source of truth that could outlive the entry it points at. `get_last_event`'s
  exception/field describers are now shared with the trace capture, so a traced hit reports exactly
  what a suspending one would. `list_stop_points` marks traced entries, and both tool descriptions now
  say that the default suspends everything.
  Validated by `traced_exception_breakpoints_…` and `traced_watchpoints_…` in `mcp_integration.rs`
  (+ `examples/probes/ExcProbe.java`, a throw-and-swallow loop): each asserts the hits land in
  `get_traces` **and** that the probe's own tick line keeps advancing — the debugger reports success
  either way, so only the debuggee's output proves nothing was left suspended.
  The `jdwp-trace` skill in the sibling repo is updated to match: Rule 0 now covers all three kinds and
  site 2's step 2 is traced. (Its "the calling stack is the one thing a trace can't give you" note was
  made wrong by TRACE-5 below, and has since been replaced there.)
- **A traced hit records who called it (TRACE-5)** — `capture_trace` read exactly one frame, so a
  logpoint could say where it fired and nothing about the path that reached it. That gap landed on the
  exact use case trace mode exists for: when you catch a swallowed exception on the shared 8180, the
  question is almost always *which request path got here*, and answering it meant giving up trace mode
  for a suspending breakpoint — the thing trace mode was introduced to avoid.
  `trace_frames` (default 3, cap 20) on all three traced stop points records that many callers above
  the hit as `class.method:line`, rendered inline on the hit's own line: `Svc.save:34 ←
  Ctl.post:40 ← Http.run:12`. **Locations only, deliberately** — the hit frame's locals are the
  payload, the callers are context, and reading every caller's variable table would multiply the
  per-hit cost on something that may fire hundreds of times. It also keeps the whole capture
  invocation-free, so caller chains work in a read-only session (SAFE-6), unlike expansion. The depth
  shows in `list_stop_points` (`[+3 caller frame(s)]`) because it is what makes a hit cost more than one
  round trip, so a slowed debuggee stays explainable from a listing; a request past the cap is clamped
  **and says so**, since a silently ignored argument would leave a caller trusting a chain they never got.
  Bug this surfaced, and the reason the test asks for a depth no path can satisfy: JDWP answers
  `INVALID_LENGTH` when a `Frames` request's length exceeds the frames a thread actually has, and a
  thread is routinely shallower than the requested depth (`main` is two frames under a helper). Asking
  for the exact count failed the whole read on those hits — losing the **locals** as well as the
  callers, silently, on precisely the shallow stacks a small depth was meant to cover. Fetching all
  frames and truncating is how `get_stack` already avoided it. Only `traced_hits_record_which_caller_…`
  asking for 3 callers where two paths have 2 caught it; a depth every path could satisfy passed.
  Validated by `traced_hits_record_which_caller_reached_them` and
  `trace_frames_zero_keeps_the_one_frame_snapshot_and_the_cap_is_reported` (+
  `examples/probes/CallerProbe.java`, whose one traced line is reached from **three** different paths —
  a single-caller probe would let a hardcoded frame pass). Each hit is paired with its own chain by the
  argument it was called with, and the probe's tick line must keep advancing, per TRACE-2.
  The `jdwp-trace` skill in the sibling `infotravel-dev-toolkit` is updated: Rule 0 now documents the
  caller chain instead of claiming a trace can't give you one, site 2 no longer sends you to a suspension
  for the originating stack, and `TECHNIQUES.md`'s "walk up the call chain one frame at a time" is gone.
  It is explicit there that caller frames carry **locations only** — the chain replaces the search for the
  next frame, not the reading of its value, which is the one way that advice could be misread.
- **Method-exit reporting, and `MethodEntry` deleted (METH-1)** — the receiving half of method events was
  built and unreachable: `EventKind::MethodEntry`/`MethodExit` existed and `handlers.rs` named both in
  `event_type_name` / `event_location` / `event_suspends`, but no tool could arm one — and the wire
  parser did not even dispatch to them, so the variants could never be constructed either. Half-built
  plumbing that implied a capability nothing had.
  Finished the useful half: `debug.set_method_exit_stop` answers **what did this method actually
  return**, and from **which `return`**. `METHOD_EXIT_WITH_RETURN_VALUE` (kind 42) carries the value and
  the hit location is the return site, so a method with several exits stops being a guessing game — the
  `IntegraSrv.post`-style bug (a non-200 path returning `null`) is exactly "which return did it take, and
  with what". Two real trace lines, from the same armed request:
  ```
  #1 [mexit_1] ReturnProbe.classify:30 ← ReturnProbe.main:42 thread=0x1 returned="OK" {n=(int) 38}
  #2 [mexit_1] ReturnProbe.classify:32 ← ReturnProbe.main:43 thread=0x1 returned=null {n=(int) 39}
  ```
  Two return sites, two values, and `null` reported as `null` rather than as an absent field.
  Deleted the other half: **there is no `MethodEntry`.** A `METHOD_ENTRY` request with a `ClassMatch`
  fires on every method of every matching class — the noisiest event in JDWP — and "what calls this?" is
  now answered far more cheaply by a traced breakpoint's caller chain (TRACE-5). Keeping a decoded
  variant nothing can arm was the exact problem this item was raised about, so it went.
  Two things make this kind's safety different from every other stop point here, and both are inverted
  on purpose: **`trace` defaults to `true`** (a suspending `MethodExit` on a hot method is the fastest
  way to freeze a shared JVM this tool offers, so the safe mode is the default and the dangerous one is
  opt-in), and **a broad suspending request is refused outright** — wildcard pattern or no `method` name
  — naming both the reason and the narrow form that would be accepted. `panic` clears method-exit
  requests before resuming, which matters more here than anywhere else: resuming with one still armed
  re-freezes the VM on the very next return.
  JDWP has **no method-name modifier**, so `method` is filtered on our side: the JVM reports every method
  of the class and non-matching exits are dropped — without recording and without charging the trace
  budget, so "exactly N traces then it stops" still holds. Both the trace path and the suspending path
  filter (the latter resuming what it drops, or a request for `save` would freeze on the first unrelated
  getter that returns). Also recorded, because it is easy to get wrong: JDI's `canGetMethodReturnValues`
  is **not** a capability bit — it is a JDWP version check (≥ 1.6) — so an older JVM degrades to a plain
  `MethodExit` (return site, no value) and says so.
  Validated by `method_exit_reports_the_value_each_return_produced` (+ `examples/probes/ReturnProbe.java`,
  whose `classify` has two returns and alternates between them, with a second returning method beside it
  so a filter that did nothing would be visible) and
  `a_broad_suspending_method_exit_is_refused_and_panic_clears_the_rest`. The kind-41/42 wire split is
  unit-tested, including two kind-42 events back to back — the second only parses if the first consumed
  exactly its own bytes, which is what catches a length mistake in the tagged-value read.
  The `jdwp-trace` skill leads site 1 (`IntegraSrv.post` returning `null`) with this tool now, because it
  needs **no line number** — and that file's line numbers drift, which the skill already warned about.
- **Thread dumps with lock ownership (DUMP-1)** — the recurring operational question about the shared
  8180 is *"it's wedged, which threads are blocked on what?"*, and this debugger could not answer it:
  `get_stack` took one `thread_id`, `list_threads` gave names and run status only, and the monitor
  primitives were **declared and never implemented** (`OWNED_MONITORS = 8`,
  `CURRENT_CONTENDED_MONITOR = 9` had no `pub async fn` behind them). So "who holds the lock this thread
  is waiting on" — the one question a deadlock investigation consists of — was unanswerable.
  `debug.thread_dump` returns every thread's stack in one call, plus per thread the monitors it holds
  and the one it is blocked entering, and **names the holder** of that monitor: `waiting to enter:
  LockB@f ← held by 0x9 "deadlock-two"`. That annotation is a free local correlation of data already
  collected, and it is the difference between two true-but-separate facts and a visible cycle. A full
  cycle *detector* can come later if it earns its place.
  The suspension design is the load-bearing part. JDWP defines a thread's frames and locks as readable
  only while it is **suspended**, so a dump of a running VM is honestly mostly unreadable — and quietly
  pausing a shared instance to make the output look complete is the SAFE-4 mistake. So: it never
  suspends on its own; `suspend:true` is an explicit request that freezes, reads, resumes via
  `resume_all_fully` and **verifies** (reporting the ADR-0003 "still suspended" case rather than a clean
  dump); a VM that is *already* suspended is read as it is and left that way, since resuming would
  discard the breakpoint state the caller is standing in and re-suspending would build a counted depth
  one resume can't undo (SAFE-7). A running thread gets its own line saying what would make it readable,
  and never renders as `(no frames)` — "unreadable" and "idle" are opposite answers on a wedged JVM.
  New in `jdwp-client`: `owned_monitors`, `current_contended_monitor`, and `capabilities`
  (`VirtualMachine.Capabilities`) so a JVM lacking `canGetOwnedMonitorInfo` /
  `canGetCurrentContendedMonitor` is reported as *"this JVM cannot"* rather than a bare
  `NOT_IMPLEMENTED` — the `set_field_watch` precedent. Worth recording: JDI's `canGetMethodReturnValues`
  is **not** a capability bit at all (it is a JDWP version check), which is why `CapabilitiesNew` is not
  implemented.
  Cost is reported (`Cost: N JDWP packet(s)`, via the PERF-1 instrument) because a dump is many round
  trips by construction — 8 threads × 3 frames measured 76 packets — and bounded by `limit` (40),
  `max_frames` (8, deliberately narrower than `get_stack`'s 20 since it multiplies by the thread count),
  `name_filter` and `package_filter`. Class names are cached across the whole dump, which is where most
  of the cost disappears on a request pool whose stacks are largely identical.
  Validated by `thread_dump_shows_stacks_and_the_deadlock_cycle` (+ `examples/probes/DeadlockProbe.java`,
  two threads taking two locks in opposite order behind a barrier so the cycle is guaranteed rather than
  raced for — the locks are distinct *classes* so a backwards pairing can't pass) and
  `thread_dump_works_read_only_and_never_suspends_on_its_own`. The probe's `main` keeps ticking
  throughout, which is what proves a suspending dump really resumed: the deadlocked pair never could.
  The capability-refusal paths are unit-tested against `render_thread_dump`, since no `HotSpot` will
  exercise them. `jdwp-trace`'s `TECHNIQUES.md` gains a "8180 is wedged" entry and `run-infotravel` a
  pointer, both with the narrowed 8180-safe invocation rather than a bare dump.
  Recorded as **ADR-0009**, including that this reads #15's "does not suspend as a side effect" as "does
  not suspend *silently*" — issue and code otherwise appear to disagree, and the interpretation is the
  load-bearing part of the design.
- **TEST-6 closed against a real WildFly (#13)** — all six assumptions accounted for, run against WildFly
  21.0.2 with a deployed servlet rather than stand-in probes. **Validated**: the thread filter against a
  24-thread `default task` pool (400 records, exactly one thread), and BP-4's by-name re-resolve across a real
  WAR redeploy — a new module classloader, so every id captured at arm time was stale, and `evaluate` resolved
  to the *new* generation. **Measured**: `thread_dump` costs 223ms held / 1,559 packets at its worst, so #17's
  provisional 2000ms budget has ~9× headroom on loopback; and trace capture costs ~0.86ms per hit, which is a
  ~720–1,160 hits/s *ceiling* rather than a percentage, because capture is serialised through one connection.
  Assumption 2 is **unreachable by design**, now tested rather than reasoned: `dt_socket server=y` accepts one
  connection per JVM lifetime, and `debug.pause` is idempotent precisely so a depth cannot accumulate through
  this tool's own front door (ADR-0003). A proposed workaround — building depth from concurrent `All`-policy
  hits — was tried and defeats itself: the VM freezes on the *first* hit, so no second thread reaches the
  breakpoint (40 concurrent requests → 1 event, 1 continue). `resume_all_fully`'s exhaustion tail is untested
  because a safety property makes it unreachable, not because coverage was missed.
  Assumption 5 (the 120s watchdog default) got evidence that **changed the conclusion**: a minimal
  investigation freezes the VM for 2–4ms and a typical one for ~55ms, but a thorough one ranged 709ms–39.4s —
  and nearly all of the outlier is one call. 120s is therefore **not** generous; it has ~3× headroom over a
  single `evaluate` on a framework object, and is currently absorbing EVAL-5 rather than accommodating human
  think-time.
  Three findings filed, none reachable with stand-in probes: **#21** (a thread-filtered stop point dies
  silently with its thread), **#22** (the trace ceiling, and that "safe on a shared instance" means unfrozen
  rather than undegraded), **#23** (`evaluate` froze a real VM for 40s in `toString()` and returned something
  indistinguishable from the cheap path — with the node budget making the *shallower* expansion 65× slower
  than the deeper one, because only the deeper one tripped it).
- **The thread filter, verified against a real pool (#13 assumption 1)** — `ThreadOnly` was only ever
  checked against `ThreadProbe`'s two dedicated, immortal threads, which leaves the interesting cases
  untested: a filter competing with *hundreds* of siblings rather than one, and a thread id that outlives
  many units of work. `examples/probes/PoolProbe.java` is a real request pool — 200 workers, saturated and
  reused across thousands of tasks, all running the same throw site — and
  `a_thread_filter_holds_against_a_real_pool_of_reused_threads` requires a filtered stop point to report
  **exactly one** thread out of the 200, with the other 199 still making progress. Assumption 1's local half
  is closed; `WildFly`'s own pool under real traffic still is not.
  Getting the load shape right was most of the work, and two earlier shapes failed **quietly** — the probe
  ran, looked healthy, and reproduced something other than a loaded pool. Submitting one task every 20ms
  left 199 of 200 threads idle and churned 300+ threads in five seconds; switching to one task per
  `Thread.sleep(1)` was meant to fix it, but Windows rounds a 1ms sleep to ~15ms, so the real rate was
  ~65/s, the pool settled at 55 of 200, and ~500 threads had been created and retired within ten seconds.
  Submitting a batch per iteration decouples the load from the host's timer granularity. The test asserts
  the pool is saturated before asserting anything about the filter, because "the filter excluded the
  others" proves almost nothing against a handful of threads.
  **This found a real bug** — [#21](https://github.com/YgorPerez/java-debugging-mcp/issues/21) (FILT-2): a
  filtered stop point stops reporting when its thread dies and `list_stop_points` still shows it armed with
  `⚡`. Unreachable with an immortal-thread probe, and routine on a pool that reaps idle workers.
- **The dump's suspension window is bounded and reported (#17, items 1–2)** — DUMP-1 made the suspension
  explicit, which was the important half, and left its *magnitude* unbounded and unreported: `suspend:true`
  held the VM for the whole collection loop, and the reply gave the packet cost but never the duration. So
  the one number that matters on a shared instance — what this diagnostic cost everyone else — was the one
  missing, inferable only from a packet count and a guess at round-trip latency.
  The reply now states the **held duration**, measured around the suspend/resume pair alone so our own
  string building can never inflate it (rendering happens after the resume, deliberately). `max_suspend_ms`
  (default 2000, `0` = unbounded) bounds the freeze: checked at the *thread boundary* so stopping never
  leaves a half-read row or holds the VM longer to finish one, and on exhaustion the dump resumes at once,
  returns what it has, and reports **`INCOMPLETE`** with the count it skipped and which knob to turn. Same
  shape as ADR-0002's trace budget — counted server-side, charged per unit of work, stop announced.
  Truncation and the resume outcome stay separate facts, because "I stopped early" and "I could not resume"
  are different problems; a truncated dump still resumes via `resume_all_fully` and still verifies, so
  ADR-0003 holds on the new early-exit path.
  Also fixed here, same renderer: a row read `[monitor, suspended]`, running two **independent axes**
  together — `monitor` is the application blocked on a lock, `suspended` is us holding it. As one
  comma-separated list it invited "suspended at a monitor", crediting the freeze to the application rather
  than to the debugger, which is exactly backwards for the state DUMP-1's readability logic keys off. Now
  `[monitor] debugger-suspended`.
  The default is **provisional**: 2000ms is picked from loopback, where a dump measured 5–10ms held and a
  round trip is sub-millisecond. Calibrating it against a real pool is part of #13, which this adds a sixth
  assumption to.
  Validated by `a_dump_reports_how_long_it_held_the_vm_and_a_budget_bounds_it` (+
  `examples/probes/ManyThreadsProbe.java`, 60 parked workers three frames deep). The budget is proven by
  making it *impossible to meet* — 1ms against 60 threads — rather than by hoping a dump was slow, and
  `main` is deliberately not one of the workers, since it is the only thread that can show a
  budget-truncated dump still resumed the VM. The monitors-only cheap mode, item 3 of #17, followed —
  see below.
- **One node budget per `get_stack` call (OBJ-3)** — `DEEP_NODE_BUDGET` was allocated fresh per
  `render_value_deep`, and `get_stack` called it once per local, so `expand_objects:true` on 20 frames
  × 20 locals could walk ~160k nodes against a possibly-shared JVM: a documented cap that bounded
  nothing. One `DeepState` now spans the whole call (`STACK_NODE_BUDGET` = 1000, larger than
  `evaluate`'s 400 because a stack legitimately expands many values, but not larger still — a node is
  roughly a line, so 1000 is already more reply than anyone wants). On exhaustion the output says so
  **once**, naming the frame and local it stopped at, and abandons the remaining frames instead of
  repeating the notice under each one. `evaluate` is untouched.
  Bug this surfaced: `get_stack {expand_objects:true}` **silently dropped the locals of every frame
  below the first**. Expanding a collection invokes `toArray`/`toString`, and JDWP invalidates a
  thread's frame ids the moment a method is invoked on it — so each later frame's id was stale, the
  read failed, and the frame printed as though it had no locals at all. Frame *indices* stay valid, so
  the id is re-read per frame on the expansion path. Both are asserted by
  `get_stack_node_budget_bounds_the_whole_call`.
- **Bounded event buffer (EVT-1)** — `last_event` was a single `Option`, so a second hit overwrote the
  first with no trace: on a busy WildFly you read whichever event landed last and had no way to know
  what you missed. Events now go into a ring buffer (`MAX_EVENTS` = 100) with a monotonic seq and an
  eviction count. `debug.get_last_event` still returns just the newest (so nothing that worked before
  changes), and adds `[pending] N older event(s)` / `[dropped] N evicted` so a caller knows to catch
  up; `limit` reads the backlog oldest-first, `drain` discards what it returned. Validated by
  `events_are_buffered_so_a_second_hit_doesnt_erase_the_first` — a breakpoint then a step, both still
  retrievable, which is exactly what the single slot lost.
- **Non-suspending trace breakpoints / logpoints (TRACE-1)** — `debug.set_line_stop {…,
  trace:true, trace_expr}` captures a snapshot (location, thread, in-scope locals/args, optional
  expression) and resumes just the hit thread (EventThread policy) — never freezes the VM. Bounded
  ring buffer (cap 500), read via `debug.get_traces {limit, clear}`. Validated — a probe looping a
  method yields N snapshots with args, loop counter strictly increasing (never frozen)
  (`examples/test_trace_bp.rs`).
- **Monitors-only thread dump (DUMP-2, #17)** — the deadlock question ("who holds what, who waits on
  whom") needs the lock graph, not the stacks, but reading every frame of every thread was the only way
  to get it. `monitors_only:true` reads the monitors and skips the frames: **245 packets / 33 ms held
  against 770 / 117 ms** for the same 60-thread dump. The point is the held time, not the packets — it is
  the window in which nobody else's request runs.
  What made this more than a flag: an omitted stack must not read as an *absent* stack. Frames now have
  **three** states, not two — read, unreadable (thread running), and deliberately not requested — and the
  third is stated once in the header rather than as an empty stack per row. An empty monitor set says
  "not requested", never "idle", because a dump that showed no contention would otherwise be evidence of
  no deadlock. `monitors_only` with `monitors:false` asks for neither locks nor stacks and is **refused**
  rather than silently corrected into one of them, and a `package_filter`/`max_frames` passed alongside it
  is reported as ignored instead of quietly having no effect.
  Validated by `thread_dump_monitors_only_omits_stacks_without_claiming_there_are_none`,
  `…_on_a_jvm_without_monitors_says_it_has_no_payload` and `…_reports_a_frame_filter_as_ignored`.
- **Stop-point vocabulary (VOCAB-1, #20)** — `breakpoint` named three different scopes depending on the
  tool: one source location in `set_breakpoint`, two things that were not locations in
  `set_exception_breakpoint` / `set_method_breakpoint`, and all four kinds in `clear_breakpoint` /
  `list_breakpoints` / `toggle_breakpoint` — while `set_watchpoint` was a stop point the word did not
  cover at all. The tools now follow `CONTEXT.md`, where **stop point** is the umbrella and
  **breakpoint** means a line breakpoint: `set_line_stop`, `set_exception_stop`, `set_field_stop`,
  `set_method_exit_stop`, `clear_stop_point`, `list_stop_points`, `toggle_stop_point`.
  This is a **breaking rename** with no aliases, taken deliberately while nothing scripted against the old
  names; arguments and the `bp_…` / `exc_…` / `watch_…` / `mexit_…` id prefixes are unchanged, so only the
  tool names move. The JDWP-level primitives (`conn.set_breakpoint`, `set_breakpoint_ex`) keep their names
  — there `breakpoint` is precise. README carries a migration note listing every old name.
- **Trace mode's throughput ceiling (TRACE-6, #22)** — the tool descriptions said trace mode was "safe on
  the shared 8180", which callers could read as free. It is *unfrozen*, not *undegraded*: capture is
  serialised, so a traced stop point tops out at **~720 hits/s** (~1160 with `trace_frames:0`) and hits
  past that queue. The descriptions now say so, with the number and what it means — a few hundred hits/s
  is nearly free, `trace_max_hits` (default 200) keeps even a hot line to a sub-second blip, and
  `trace_max_hits:0` makes the degradation sustained rather than bounded. Documentation only; no
  behaviour change.
  `an_unbounded_trace_budget_is_warned_about_rather_than_passed_over` covers the `trace_max_hits:0` case,
  since an unbounded budget is the one way to turn a blip into a permanent slowdown.
- **The test harness could not find a Windows JDK (test integrity)** — `Jdk::find` built
  `$JAVA_HOME/bin/java` with no extension and then asked the filesystem whether it existed. On Windows the
  files are `java.exe` / `javac.exe`, so the check always failed, `find` returned `None`, and because a
  missing JDK **skips rather than fails**, the entire `--ignored` suite reported `ok` in **0.00s while
  running nothing**. The same shape as the SIGKILL coverage bug TEST-5 found: a green run that proves
  nothing. `Jdk::in_bin` now appends the platform suffix for the existence check, while the bare `java` /
  `javac` PATH fallback deliberately stays unsuffixed because it goes through `CreateProcessW`/`execvp`,
  which resolve the extension themselves. With this fixed, **47/47 pass in 195s** serially
  (`--test-threads=1`) against real JVMs — the duration is the tell, so check it and the absence of
  `SKIP` lines before believing a green run. `scripts/integration-test.sh` no longer leaves that to the
  reader: a `SKIP …no JDK found` line is now a hard failure there, matching the guard `coverage.sh`
  already had.
  A **second** green-run-of-nothing turned up immediately afterwards, in that script's own usage line.
  It already supplies libtest's `--`, so the documented `integration-test.sh -- --test-threads=1` made
  libtest read the bare `--` as a test-name *filter*: `0 passed; 47 filtered out`, exit 0. Usage line
  fixed, and "0 tests ran" is now a hard failure too — an empty selection is a failed request, not a
  pass. Three ways to get a green run that executed nothing, all found in one sitting: SIGKILL'd counters
  (TEST-5), an undetectable JDK, and a filter that matches no tests.
- **Zero doctor warnings, and a gate that keeps them there (LINT-1, #18)** — `7253499` reached 0 warnings
  deliberately; twenty-four commits later `main` reported 7, and the four over-threshold handlers were
  described as pre-existing debt the new work merely "matched" rather than as the regression they were.
  Nothing enforced the zero, so nothing noticed — and what decayed first was the memory of having paid
  for it. Back to **0** (score 100), and CI now gates on `--fail-on warning` with a **pinned** toolchain,
  because an unpinned `stable` plus a warning gate means a future clippy breaks the build on code nobody
  touched — which is exactly how the `--fail-on error` compromise arose. ADR-0007 records the rejected
  options, including the deliberate contrast with the standing decision *not* to gate on coverage: a
  coverage percentage rises with tests that assert nothing, whereas a doctor warning is a specific finding
  at a specific line.
  The five findings were fixed by extraction, the way the previous batch fixed its own — `StackWalk` /
  `render_stack_frame` out of `handle_get_stack`, `set_field_by_path` out of `handle_set_value`,
  `watch_kinds` / `arm_one_field_watch` / `render_field_stop_reply` out of `handle_set_field_stop`,
  `render_exception_stop_reply` out of its handler, and `dump_filter_note` / `dump_monitor_caveats` out of
  `render_dump_header` (which this batch had itself pushed to 20). The three `.clone()`-in-a-loop findings
  were **restructured rather than relocated**: the exhausted-local name is copied once on the way out
  instead of per frame, and a filtered map's surviving keys are moved out of the scan rather than cloned
  from a shared vector per match. Moving an allocation somewhere the heuristic cannot see it would have
  scored the same and fixed nothing.
- **A traced stop point reports what it actually costs (TRACE-7, #26)** — item 4 of #22, the observation
  half of what that issue documented. #22 put ~0.86 ms per hit and a ~720 hits/s ceiling in the tool
  descriptions; true figures, from one measurement, on one machine, against one endpoint. What a caller
  needs is what **their** stop point on **their** site costs, and the debugger was the only thing that could
  answer — it already counted hits for `trace_max_hits`, so it only lacked a clock. `list_stop_points` now
  reports, per traced stop point: mean capture (invert it for the rate past which hits queue, which is the
  form #22's figure is quoted in), the rate hits are **arriving** at, and the share of the
  window spent capturing — which is the number that answers "is this hurting the instance?", since a cheap
  capture on a hot line and a costly one on a quiet line are not the same problem. Same move #17 made for
  `thread_dump`'s held duration, and measured the same way: the timer wraps the **capture only**, never the
  budget arithmetic or the resume, so our own bookkeeping cannot inflate the price we then blame on
  tracing. A traced stop point with no hits reports UNMEASURED rather than `0.00ms` — a rounded-down zero
  would read as free. A suspending one reports nothing at all, because it captures nothing; its price is the
  freeze. Re-arming resets the figures (a disabled gap would otherwise dilute the arrival rate into
  nonsense). Recorded as **ADR-0010**, with the rejected alternatives — timing the whole pump iteration,
  acting on the number rather than reporting it, and a separate `debug.trace_cost` tool.
  Validated by `a_traced_stop_point_reports_its_observed_capture_cost` against `CallerProbe`, which reaches
  the traced line **three times per ~150 ms iteration** — so the arrival rate is known independently and the
  test asserts the reported numbers land on it (~20/s) rather than merely being present. It measured
  **1.65 ms mean / 20.5 hits/s arriving / 3.4% of the window** here — 1/1.65 ms being ~608 hits/s, which
  corroborates #22's ~1.39 ms and ~720/s on slower hardware, the first time those figures have been checked
  by anything but the measurement that produced them.
  A fourth figure, `sustains ~N/s`, was reported at first and then **cut**: being exactly 1/mean it was the
  only number on the line that restated another instead of measuring something, and two differently-scoped
  "rates" made a reader establish which was which before either helped. #26's own acceptance criteria asked
  for both senses — "the observed hit rate" in one clause, a rate reflecting "the capture window only, not
  idle time between hits" in another — so the first cut reported both rather than choosing. ADR-0010 records
  the resolution: the mean is measured, the ceiling is arithmetic on it.
- **The stdio front door, covered — and it was hiding a hang (TEST-9, #25)** — `main.rs` sat at 65% region
  because every test in the suite constructed a **valid** request, so the parsing between a buggy client and
  the debugger was the one thing nothing drove. `mcp-server/tests/stdio_protocol.rs` drives it with input a
  client should never send. The find was not a percentage: a top-level JSON value that is **not an object**
  — `42`, `"hello"`, a batch array — has no `id`, so it fell into the notification branch, failed to parse
  as one, and was answered with **nothing at all**. Valid JSON, so not a parse error; not an object, so not
  a request. A client waited forever for a reply that was never coming, which is worse than any error code.
  It now answers `INVALID_REQUEST` with a null id, as JSON-RPC 2.0's own example for a non-object does,
  while a genuine notification's silence — which is *correct* — is left alone.
  Seven tests, and each asserts the property that actually matters: an error came back **and the server is
  still serving**, because one bad line must not end the session. They cover unparseable JSON, a
  non-object, an object with no `method`, a non-string `method`, an unknown method, an odd-but-legal `id`
  shape, notifications and blank lines (proved silent by ordering, not by a timeout), EOF as a clean exit,
  and a final request with no trailing newline — which is answered *at* EOF, since `read_line` holds a
  partial line until the newline or the close. **No JDK needed**, so they are not `#[ignore]`d: hiding them
  behind the flag that exists for JVM tests would put them behind a gate they don't need.
- **A production-shaped dump cost 13× more packets than it needed to (TEST-8, #24, partial)** — and the
  reason nobody knew is that "we need the real 8180" had been accepted as the answer. It is not. Of the
  three things that make the real instance different, **two are properties of the debuggee**: thread count
  is a loop bound and stack depth is a call chain. `PoolShapeProbe` presents them (300 workers, 60 distinct
  frames, parked, named like a real pool); `LatencyRelay` supplies the third in userspace, since
  `tc … netem` needs `NET_ADMIN` a container lacks.
  Measured against that shape on loopback, a whole-pool 60-frame dump cost **21,364 packets / 4,686 ms**,
  and **at the default 2000 ms budget it truncated at 40% of the pool**. ~19,000 of those packets were
  `Method.LineTable` — asked once per frame per thread while covering ~60 distinct methods, because the
  threads of a request pool are all standing in the same code. Method *lists* were already cached on the
  connection; line tables were not. Cached per dump: **1,625 packets / ~700 ms**, and the same dump now
  completes *inside* the existing budget.
  The relay then settled what the wire contributes: 0/1/2/4 ms round trip over one workload gave **~1.0 ms
  of held time per ms of RTT per packet** (slope 0.997), against a raw loopback TCP round trip of 0.048 ms
  and ~0.22 ms of our own per-packet cost. So `held ≈ packets × (ours + RTT)`, which is why the fix was
  fewer packets rather than a longer freeze — on an instance 1 ms away that dump would have frozen the VM
  for ~26 s. **`max_suspend_ms` stays 2000**, deliberately: it is a safety net, and its truncation was the
  net working. `limit` 40 / `max_frames` 8 stay too — reviewed against a 306-thread, 60-frame pool where
  they cost 258 packets and ~65 ms, and their binding constraint was never round trips but how much stack a
  reader can use. Recorded as **ADR-0011**, which also explains why a *connection*-level cache is still the
  rejected option (a redefined class keeps its type id, so a stale line is worse than a round trip) and why
  `monitors_only` was not the answer — it is ~1.3× cheaper now rather than ~18×, which is the honest
  position for a mode that answers a different question.
  **And the win does not depend on the pool being uniform**, which is the obvious objection to measuring it
  against 300 threads in identical code. Cost is `threads × fixed + distinct (class, method) pairs`, so
  diversity is paid for per distinct frame rather than per thread: `MixedPoolProbe` (300 workers, 10
  handlers, a shared 40-frame framework prefix — 240 distinct pairs instead of 60) costs **1,812 packets**
  against 1,625 uniform and 21,364 with nothing shared. +187 for +180 extra pairs, one packet each, which is
  the model exactly. A real request stack is mostly shared framework with a handler at the bottom, so it
  sits in that middle row.
  Four tests, and the cost one asserts **packets per thread (≤20), not a duration**: a packet count is
  deterministic and load-independent, and it fails at ~70 with the cache defeated — verified by defeating
  it. `a_deep_dump_resolves_each_frames_own_source_line` checks all 59 chain frames against the probe's own
  source, because a cache keyed too coarsely still produces a plausible dump;
  `one_cached_line_table_resolves_each_bytecode_index_to_its_own_line` covers the case no probe can
  construct on demand; `latency_added_to_the_wire_shows_up_as_held_time_per_packet` keeps the relay honest,
  since a relay that silently stopped delaying would make every measurement through it worthless.
  **The other half of #24 was "read the real instance's parameters and do the arithmetic", so the dump does
  it instead.** The cost line now carries its observed per-packet price — `Cost: 258 JDWP packet(s), 3.13ms
  each` — which *is* the RTT term for whatever instance is attached, and a truncated dump reports what
  finishing would have cost at the rate it ran: `at 18.6ms per thread, the 198 threads it skipped need
  ~3677ms more — about 5682ms for the whole set`. Both extrapolate from the packet counter and the held
  clock that were already there; neither predicts (see ADR-0011 for why a *pre*-dump prediction is a range,
  not a bound). A default calibrated against one instance is a guess about every other one; a default plus a
  reply stating what *this* instance costs needs no calibration.
  Swept with the relay, the defaults hold the VM inside the 2000 ms budget **up to roughly a 6 ms round
  trip**, and truncate past ~7 ms — 0.36 ms/packet and 89 ms held on loopback, 6.19 ms and 1,564 ms at 5 ms
  RTT, truncating at 34 of 306 threads by 8 ms. So 2000 ms is right for a LAN-local instance, and on a
  slower link even a defaults dump truncates, which is the net working and now says what finishing needed.
- **`get_id_sizes` deleted (CLEAN-1, #27)** — the only function in the #19 coverage review with **zero hits
  anywhere**. Its one caller was `examples/test_vm_commands.rs`, a manual harness nothing runs, which is
  why it measured zero. Deleting it was the point: the reader assumes 8-byte ids outright, and an uncalled
  `IDSizes` wrapper made that assumption look **checked** when nothing checked it. The assumption is now
  stated where the reader relies on it, explicitly as unvalidated, along with what a real check would have
  to be (a refusal at attach time) and what a mismatch would look like without one (misaligned reads
  surfacing as garbled values, not as a clear error). `vm_commands::ID_SIZES` stays: it is one row of a
  complete spec-derived constant table, most of which is unused by design.

---

## Backlog

**Eleven open, from three sources.** Tracked as GitHub issues, not here.

**From #17–#22's evidence — two open, one of them nearly closed.** Three of the five have shipped
(#25, #26, #27), and most of #24 turned out not to need what it said it needed — see the shipped entries
above.

| issue | why it exists | what is actually left |
| --- | --- | --- |
| [#24](https://github.com/YgorPerez/java-debugging-mcp/issues/24) TEST-8 · P1 | Successor to the closed TEST-6/#13. Every shared-instance default (`max_suspend_ms` 2000, `limit` 40, `max_frames` 8) was calibrated on loopback against probes, and the monitors-only saving was measured at 3 frames deep. | **Done except for taking the reading.** Thread count and stack depth are debuggee properties (`PoolShapeProbe`), latency is injectable (`LatencyRelay`), the 13× packet waste that made the defaults look wrong is fixed, all three defaults were reviewed and kept with measurements, and the dump now reports its own per-packet cost and what a truncation would have needed — so the calibration step is a normal dump rather than an exercise (ADR-0011). Left: run one against the real 8180 and confirm the freeze policy against what it says. |
| [#28](https://github.com/YgorPerez/java-debugging-mcp/issues/28) LINT-2 · P2 | The #18 gate's maintenance debt: a pinned toolchain with no bump trigger, and per-crate `clippy.toml` that a third crate would not have. | Item 1 is a policy call about cadence and noise tolerance; item 2's best fix depends on the answer. |

**From a tool-surface comparison against [`d4n-sec/jdb-mcp`](https://github.com/d4n-sec/jdb-mcp)
(2026-07-26) — five filed, one rejected.** A JDI-based Java debugger MCP server; the first batch not produced by
reviewing this codebase. Eleven candidate features, six real gaps, and they share a theme worth stating
plainly: **every stop point in this server is addressed by a name the caller has no way to look up.**
Two of the six are the same shape METH-1/#16 found — implemented in `jdwp-client` and unreachable from
MCP. Each carries an agent brief.

| issue | why it exists | what is actually left |
| --- | --- | --- |
| [#29](https://github.com/YgorPerez/java-debugging-mcp/issues/29) DISC-1 · P1 · S | No class discovery. On the shared 8180 a generated proxy or a shaded class is not findable from the source tree at all — only the debuggee knows what it loaded. | `all_classes` is implemented and reachable only from `examples/test_find_class.rs`. Left: the tool, JNI-signature-to-FQN rendering, and bounding the output on a server with thousands of loaded types. |
| [#30](https://github.com/YgorPerez/java-debugging-mcp/issues/30) DISC-2 · P2 · S | No method listing, so a caller composing `debug.evaluate` cannot see the parameter lists they are trying to satisfy — the most intricate part of this server, driven blind. | `ReferenceType.Methods` is implemented and already consumed internally by overload resolution. Left: the tool, and rendering JVM signatures as Java source types. |
| [#31](https://github.com/YgorPerez/java-debugging-mcp/issues/31) DISC-3 · P2 · M | A stop point reports `class.method:412` and nothing can read line 412 — or confirm the local checkout is the deployed build, which is the assumption that wastes an hour mid-investigation. | All of it. No `ReferenceType.SourceFile` command exists; `Method.LineTable` gives numbers only. Two halves — ask the debuggee what it compiled from, then read it from source roots. |
| [#32](https://github.com/YgorPerez/java-debugging-mcp/issues/32) EVT-2 · P2 · M | Hits are discoverable only by polling, so the watchdog's 120s budget burns while nobody is looking. MCP already has the mechanism. | The buffer stays regardless — notifications are best-effort. Constraints in the brief: suspension only (a traced hit at ~720/s would flood the transport), bounded repeats per SAFE-8/#8, and the watchdog's auto-disarm is worth pushing too. |
| ❌ [#33](https://github.com/YgorPerez/java-debugging-mcp/issues/33) TRANS-1 · P3 · M | stdio-only. | **Closed `wontfix`** — see `.out-of-scope/http-transport.md`. The "what is it *for*" question did have an answer, and it was not HTTP: co-locating the debugger with the debuggee, because TRACE-6's ~720 hits/s is a round-trip limit. Running the stdio binary near the JVM buys that for nothing. What killed it is that client lifetime **is** session lifetime here — SAFE-1's disconnect and EVT-2's single writer both assume it — so an HTTP client closing its laptop mid-suspension makes the watchdog load-bearing for routine disconnects. |
| [#34](https://github.com/YgorPerez/java-debugging-mcp/issues/34) REL-1 · P3 · S | Installing needs a Rust toolchain, which hands back most of the no-JVM argument for writing JDWP natively — a Java developer is the person least likely to have `cargo`. | All of it. No release workflow exists; `Cargo.toml` already carries the metadata. Should reuse the existing toolchain pinning rather than adding a second, per LINT-2/#28. |

**From TEST-8's method — three open.** #24 was labelled `ready-for-human` because it needed the shared
8180, and most of it turned out not to. The generalisation is worth keeping: *"we cannot test that" is
usually "we have not built the instrument"*. Four were proposed; the first shipped — `FaultRelay`, a
fault-injecting JDWP proxy, which immediately reached `resume_all_fully`'s honest-failure tail that the
coverage review had called unreachable through this tool's own API. These are the other three.

| issue | why it exists | what is actually left |
| --- | --- | --- |
| [#35](https://github.com/YgorPerez/java-debugging-mcp/issues/35) TEST-10 · P2 · S | Every probe is well-behaved, so four real debuggee states are never presented: threads dying mid-dump (the `collect_dump_rows` arm for it is unexercised), contention beyond `DeadlockProbe`'s two threads, synthetic/lambda class names, and most `ValueData` variants — which is *why* `types.rs` sits at 16.67%. | All of it, but cheap: probes only, no new infrastructure. |
| [#36](https://github.com/YgorPerez/java-debugging-mcp/issues/36) TEST-11 · P2 · S | The suite runs on JDK 21; **the 8180 runs JDK 11**, and nothing had ever run against it. | **Answered, and it found something on the first run.** 50 of 53 tests failed on 11 — not a JDWP difference but `javac` reading source in the platform charset before JEP 400, so every probe comment with an em dash failed to compile. One `-encoding UTF-8` later: **53/53 green on 11 and on 21**, so there is no incompatibility to fix. Left: the CI matrix that keeps it that way. A matrix still can**not** reach the `JDWP < 1.6` path — JDWP tracks the JDK, so 11 speaks 1.11; that needs `FaultRelay` or #37. |
| [#37](https://github.com/YgorPerez/java-debugging-mcp/issues/37) TEST-12 · P2 · M | #24's residue is one human dump against the real instance, after which the evidence evaporates. Recording the JDWP stream once turns that visit into a permanent CI fixture — replayable with no JVM and no access, and hand-editable into shapes nothing can produce. | The machinery, tested against probe recordings first. `FaultRelay` already frames JDWP; this is the third user of that framing and the point to unify the two proxies rather than add a third. |

The comparison also produced two **rejections**, both recorded in `.out-of-scope/` so the reasoning
outlives the issues. `method-entry-events.md`: `METHOD_ENTRY` stays unarmed for the reasons METH-1/#16
settled — it fires on every method of every matching class, and the caller chain from TRACE-5/#14
answers the same question at one site without suspending. `http-transport.md`: TRANS-1/#33 above, where
the recorded part worth keeping is the *motivation* — latency to the debuggee — since that is what
would legitimately reopen it, and it has a cheaper answer than a second transport.
Four other candidates were checked against the code and not filed at all:
`smartStep` (already the last-hit-thread default), JDK 7 support (n/a on the wire protocol),
`get_output`/`send_input` (no process handle on an attach-only connection — dead code upstream too),
and bilingual docs.

A **fifth** review found three more, shipped as issues
[#7–#9](https://github.com/YgorPerez/java-debugging-mcp/issues?q=is%3Aissue). The headline one was
verified against a real JVM before being filed, not reasoned about:

- **SAFE-7** (#7) — JDWP *counts* suspends, and nothing here knew that. `debug.pause` never checked
  whether the VM was already stopped, so pausing at a breakpoint (or twice) built a depth that one
  `resume_all` couldn't undo — and the watchdog then reported "auto-resumed", cleared `suspended_since`,
  and never retried: **frozen permanently, reported rescued**. Measured on a real JVM: two pauses then one
  continue left the probe at 0 ticks; the second continue released it (+14). `SuspendCount` (declared but
  never implemented) now exists; `pause` is idempotent; `continue`/`panic`/watchdog resume until the VM
  really runs and say so if they can't. Pausing at a breakpoint also no longer overwrites the
  `StopPoint` cause, which had silently lost the SAFE-2 disarm.
- **SAFE-8** (#8) — `trace_disarms` was the one unbounded buffer in a session. Harmless while an
  auto-disarm deleted the stop point; BP-2/BP-3 made re-arming easy, so one logpoint can disarm
  repeatedly. Repeats now collapse into a count, capped, with drops reported.
- **BP-4** (#9) — re-arming trusted the JDWP ids captured when the stop point was first armed. Those are
  only valid while the type stays loaded, and the realistic sequence on an app server is "disable,
  redeploy, re-arm". Re-arm re-resolves by name now, and says so plainly when the class has gone.

A **fourth** review (of the third batch) found five more, tracked as
GitHub issues [#2–#6](https://github.com/YgorPerez/java-debugging-mcp/issues?q=is%3Aissue) rather than
inline here. Three were gaps in the third batch itself, which is the useful part: the interesting bugs
were in the safety features, and two of them had green tests.

- **SAFE-4** (#2) — `debug.pause` suspended every thread and recorded nothing, so `suspended_since`
  stayed `None` and the watchdog never fired. A forgotten pause froze the JVM permanently — the same
  hazard SAFE-1 fixed for disconnect, in the tool whose name sounds harmless. Predated the batch.
- **SAFE-5** (#3) — the watchdog re-derived the offending stop point from the newest buffered event,
  which `get_last_event {drain:true}` erases; so the polling caller `drain` exists for was exactly the
  one whose freeze was resumed but never disarmed. The cause is recorded at suspension time now
  (`SuspendCause`), which also lets a manual pause be told apart from a hit.
- **SAFE-6** (#4) — read-only was enforced by inspecting expression text, which missed every indirect
  invocation: `toString()` rendering (so `evaluate {"order"}` ran debuggee code), `List`/`Map`
  subscripts, `valueOf` boxing, conditions and `trace_expr`. Now enforced on the **connection**, so a
  new expression form can't bypass it. The old test passed because it only tried field reads and an
  explicit call.
- **BP-2** (#5) — an automatic disarm (watchdog or trace budget) deleted the stop point, destroying the
  condition/`trace_expr` it was meant to protect. It disables instead, for all three kinds, so one
  toggle re-arms it.
- **BP-3** (#6) — stop-point ids embedded the JDWP request id, so re-arming minted a new id and broke
  any id the caller held; and toggling a deferred breakpoint said "not found" for an id
  `list_stop_points` was displaying. Ids come from a per-session counter now.

Two of the new tests were only load-bearing after being made so: the SAFE-4 test was verified to fail
without its fix, and the SAFE-5 test passed against the bug twice — first because the watchdog raced
ahead of the drain, then because the still-armed breakpoint re-froze and a *second* watchdog cycle
disarmed it, making the listing look identical. It now asserts the probe's tick *rate* after the resume,
which is the only thing that separates "disarmed" from "re-froze and got disarmed later".

The third review's ten items — the "shared-JVM safety was the least-finished part" batch —
have all shipped. Each is recorded in **✅ Shipped (context)** above with a `path`/test citation, and
each ships with an automated test (unit for the pure logic, MCP-level integration for the runtime
behaviour) driven against a real probe JVM the way `docs/agents/domain.md` and the house pattern
require. The former backlog entries are preserved below, collapsed, as the record of what was asked for
versus what was built.

Priority key: **P1** = highest payoff for the infotravel/integraWS investigations (shared 8180 +
silent-failure debugging); **P2** = solid follow-ups; effort is rough (S/M).

> The first ten items shipped, then a review produced ten more (TRACE-2, OBJ-3, EVT-1, SESS-1, EVAL-3,
> OBJ-4, PERF-1, TEST-2, DOC-2, TEST-3), which also shipped. A **third** review produced the ten below —
> and they shared a theme worth stating plainly: **the parts meant to keep a shared JVM safe were the
> least finished parts.** All ten have now shipped too; the theme is closed.

### ✅ SAFE-1 — `debug.disconnect` no longer leaves the JVM frozen  · P1 · S

`handle_disconnect` now resumes the VM and clears every event request **before** dropping the session,
via the new `JdwpConnection::dispose()` (VirtualMachine.Dispose, cmd set 1 cmd 6) — the JVM's own
"resume all, clear all" — with a fallback to `clear_all_breakpoints` + `resume_all` on a half-dead
socket. The reply says what it resumed/cleared and whether the VM had been suspended. Validated by
`disconnect_resumes_and_clears_instead_of_freezing` (watchdog disabled, so only the disconnect can
rescue the VM; the probe's own ticks resuming is the proof).

### ✅ SAFE-2 — the watchdog disarms the offending stop point  · P1 · S

On timeout the watchdog now identifies the request that suspended the VM (the newest buffered event's
`request_id`), disarms **only** that stop point (`disarm_request`, `handlers.rs`), and records a note
surfaced in `list_stop_points` and the next `get_last_event` — so the cycle is no longer freeze → 120s →
resume → freeze again. Unrelated stop points survive. Validated by
`watchdog_auto_resumes_and_disarms_the_offending_breakpoint`.

### ✅ TEST-4 — the watchdog has tests  · P1 · S

Three MCP-level tests with `JDWP_WATCHDOG_SECS=1`/`0`: auto-resume proven by the probe's own output
(`watchdog_auto_resumes_and_disarms_the_offending_breakpoint`), `=0` disables it
(`watchdog_zero_disables_the_auto_resume`), and a pending single-step is cleared by the resume
(`watchdog_clears_a_pending_single_step`).

### ✅ TRACE-3 — a traced hot stop point can't flood the debuggee  · P1 · M

Every traced stop point carries a hit budget (`trace_max_hits`, default 200; `0` = unbounded). Each
recorded hit decrements it (`charge_trace_budget`), and on reaching zero the request disarms itself and
`get_traces` says so — silence never reads as "no hits". `list_stop_points` shows the remaining budget.
(Server-side counting rather than JDWP's `Count` modifier, because `Count` reports only the *Nth*
occurrence, not the first N.) Validated by `trace_budget_disarms_and_get_traces_filters`.

### ✅ FILT-1 — thread filter on exception breakpoints and watchpoints  · P1 · M

`set_exception_request_ex` / `set_field_watch_ex` accept an optional `ThreadOnly` (modKind 3) modifier;
`set_exception_stop` and `set_field_stop` take a `thread_id`, honoured by the JVM and composing
with `trace:true`. The tool descriptions document the `list_threads {name_filter}` → arm → trigger flow.
Validated by `thread_filter_reports_only_the_chosen_thread` (a two-thread probe; only the filtered
thread's throws are recorded, and the other keeps running).

### ✅ EVAL-4 — `&&` / `||` in conditions and predicates  · P2 · M

Both breakpoint conditions and `[?…]` predicates parse into a boolean tree (`parse_bool_tree`) with `||`
lower precedence than `&&`, parentheses regrouping, and short-circuit evaluation; the OBJ-2
"resolve the element-independent side once" optimisation is kept per leaf. Validated by
`boolean_operators_in_predicates_and_conditions` (incl. an `a || b && c` precedence case and a
parenthesised regroup) and unit tests for the parser.

### ✅ BP-1 — `debug.toggle_stop_point` implements `enabled`  · P2 · S

`enabled` is now real: disabling clears the JDWP request but keeps the definition (condition, trace_expr,
resolved location in a `BreakpointArm`), and enabling re-arms at the same place. `list_stop_points` marks
a disabled breakpoint. Validated by `toggle_stop_point_disables_and_rearms`.

### ✅ TRACE-4 — `get_traces` can be filtered  · P2 · S

`get_traces` gained `bp_id`, `class_filter`, and `since` (sequence number, for polling); the header still
reports total vs shown. Validated by `trace_budget_disarms_and_get_traces_filters`.

### ✅ SETF-2 — `set_value` writes expressions, not just literals  · P2 · M

`value_to_write` resolves an expression right-hand side (`this.a = other.b`) and validates its runtime
type against the target's declared type (the EVAL-3 `implements_interface` assignability check, interfaces
included); a mismatch is refused, naming both types. Literals are unchanged. Validated by
`set_value_copies_a_live_reference_and_refuses_a_mismatch`.

### ✅ SAFE-3 — read-only mode  · P2 · S

`JDWP_READONLY=1` (or `read_only:true` on `attach`) refuses `set_value`, `force_return`, and method
invocation in `evaluate`; reads that need no invocation still work, and `expand_objects` falls back to
shallow with a note. `list_sessions` flags read-only sessions. Documented as a guard against accident,
not a security boundary. Validated by `read_only_refuses_mutation_but_allows_reads`.

---

<details>
<summary>Original third-review backlog entries (all shipped — kept as the record of ask vs. build)</summary>

### SAFE-1 — `debug.disconnect` can leave the JVM frozen forever  · P1 · S

**What to build**
`handle_disconnect` (`handlers.rs:747`) calls `remove_session`, which aborts the event listener **and the
watchdog** and drops the session (`session.rs:269`). It never resumes anything. So: hit a breakpoint,
call `debug.disconnect`, and every thread stays suspended with nothing left alive to rescue it — the
watchdog that would have auto-resumed after 120s was just killed on the way out.

On the shared 8180 that is the worst outcome in this project's threat model, produced by the tool whose
name sounds like the safe way out. The tell: `Server`'s `Drop` in `tests/common/mod.rs` calls
`debug.panic` before killing the child — the harness knows to do this and the tool doesn't.

**Shape of the change**
`VirtualMachine.Dispose` (command set 1, command 6) is defined to clear every event request and resume
every thread; the constant is already in `commands.rs:34`, unused. Add `dispose()` to `jdwp-client` and
call it before dropping the session. Failing that, at minimum clear stop points + `resume_all` — but
`Dispose` is the JVM's own answer and cannot leave a request behind.

**Acceptance criteria**
- [ ] `debug.disconnect` on a session suspended at a breakpoint leaves the JVM **running**
- [ ] No event request survives the disconnect (a re-attach sees a clean VM)
- [ ] Integration test: break, disconnect, then `Probe::wait_for_line` proves the probe resumed printing —
      the debuggee's own output is the only thing that proves this, per the TRACE-2 pattern
- [ ] The reply says what it resumed/cleared, so the caller knows the JVM was left safe

**Blocked by**
None.

### SAFE-2 — the watchdog treats the symptom and leaves the cause armed  · P1 · S

**What to build**
The watchdog (`handlers.rs:2385`) clears the pending step, calls `resume_all`, and stops. The breakpoint
that froze the VM is still armed, so on a hit endpoint the cycle is freeze → 120s → resume → freeze again
on the very next request, indefinitely — each round burning two minutes of everyone else's requests.

It should disarm what caused the suspension (or convert it to a `trace:true` logpoint, which is the same
information without the freeze) and report which stop point it killed and why.

**Shape of the change**
The watchdog knows `last_event`, so it can identify the request id that suspended the VM and clear that
one rather than everything. Prefer surgical: clearing every stop point on a timeout would silently throw
away a careful setup.

**Acceptance criteria**
- [ ] After a watchdog resume, the offending stop point no longer suspends the VM
- [ ] What was disarmed is discoverable — in `list_stop_points` and in the next `get_last_event`/log line
- [ ] Unrelated stop points survive
- [ ] Integration test with `JDWP_WATCHDOG_SECS=1`: a probe hitting a breakpoint in a loop keeps running
      after one watchdog cycle instead of re-freezing

**Blocked by**
None. Shares its test harness with TEST-4.

### TEST-4 — the watchdog has no tests at all  · P1 · S

**What to build**
Zero mentions of the watchdog in either test file. It is the primary safety mechanism of the whole
project — the thing that makes attaching to a shared instance defensible — and the only subsystem with no
coverage. `JDWP_WATCHDOG_SECS` is already an env var, so a test can set it to 1 second.

**Acceptance criteria**
- [ ] A suspended VM is auto-resumed after the configured timeout, proven by the **probe's own output**
- [ ] `JDWP_WATCHDOG_SECS=0` disables it (documented behaviour, currently unverified)
- [ ] A pending single-step request is cleared by the resume (it must be, or the next resume fails)
- [ ] The test doesn't add 120s to the suite — it sets the timeout to ~1s

**Blocked by**
None. Do it before SAFE-2 so the fix has something to prove itself against.

### TRACE-3 — a traced hot field can flood the debuggee  · P1 · M

**What to build**
TRACE-2 made `trace:true` available on watchpoints and exception breakpoints, which means it is now easy
to arm something that fires thousands of times a second. Every hit costs a `get_frames`, a variable
table, a `get_frame_values` and the describers (`capture_trace`, `handlers.rs:4702`). `MAX_TRACES` bounds
**memory**, not per-hit work in the target.

So the mode advertised as "safe on a shared instance" can degrade the app *worse* than a suspending
breakpoint, which at least stops at the first hit. This is a gap introduced by TRACE-2 and should be
closed by whoever relies on it.

**Shape of the change**
A hit budget per stop point, with auto-disarm and a note in `get_traces` saying it disarmed itself.
JDWP's `Count` modifier (modKind 1) enforces a limit *inside the JVM*, which is strictly better than
counting on our side — no packet is sent at all once it expires. Consider also a cheap mode that records
only the location and skips the frame walk.

**Acceptance criteria**
- [ ] A traced stop point disarms itself after N hits (default documented; overridable)
- [ ] `get_traces` says a stop point stopped recording, and why — silence must not read as "no hits"
- [ ] Integration test: a probe writing a field in a tight loop yields exactly N traces, then the probe
      **speeds back up** (measurable from its own output rate)
- [ ] `list_stop_points` shows the remaining budget

**Blocked by**
None.

### FILT-1 — no thread filter on exception breakpoints or watchpoints  · P1 · M

**What to build**
`thread_filter` is threaded only into `set_breakpoint_ex`; `set_exception_request`
(`eventrequest.rs:156`) and `set_field_watch` (`eventrequest.rs:212`) accept no modifiers beyond their
own. On a WildFly with hundreds of threads, restricting a stop point to **your** request thread is the
single largest noise reduction available — and it composes with trace mode, which is exactly the
combination the infotravel investigations want: catch only the throws from the request you just made.

**Shape of the change**
`ThreadOnly` is modKind 3; add it to `mod_kinds` (`eventrequest.rs:24`) and accept an optional
`thread_id` on both tools. The hard part is ergonomic, not protocol: you need the thread id *before* the
event, so document the flow (`list_threads {name_filter}` → arm → trigger).

**Acceptance criteria**
- [ ] `thread_id` on `set_exception_stop` and `set_field_stop`, honoured by the JVM
- [ ] A probe throwing on two threads reports only the filtered one
- [ ] Composes with `trace:true`
- [ ] The tool descriptions explain how to get a thread id first

**Blocked by**
None.

### EVAL-4 — no `&&` / `||` in conditions or predicates  · P2 · M

**What to build**
`split_comparison` (`handlers.rs:4819`) recognises only the six comparison operators, so
`[?paid == true && qty > 3]` and `condition: total > 100 && status == "OPEN"` are both unavailable.
Today you filter twice or pick one clause — for a conditional breakpoint there is no "filter twice".

**Shape of the change**
Split on `&&`/`||` at bracket/quote depth 0 *before* splitting comparisons, and evaluate left to right
with short-circuiting (which also keeps the round-trip cost down — the second clause is only resolved if
the first holds). Keep the element-relative left side working per clause, and keep the existing
"resolve the element-independent side once" optimisation from OBJ-2.

**Acceptance criteria**
- [ ] `&&` and `||` in both breakpoint conditions and `[?…]` predicates, short-circuiting
- [ ] Precedence is documented and tested (`a || b && c`), or parentheses are required and enforced
- [ ] A clause that fails to evaluate is reported as an error, not silently false
- [ ] Probe coverage for a two-clause predicate that matches a different set than either clause alone

**Blocked by**
None.

### BP-1 — `enabled` is dead state the UI advertises  · P2 · S

**What to build**
`BreakpointInfo.enabled` (`session.rs:180`) is set to `true` at both construction sites and never
mutated, yet `render_breakpoint_line` prints `✓`/`✗` from it — so the `✗` branch cannot happen and the
tick mark carries no information.

Either implement it (`debug.toggle_stop_point`, which is genuinely useful — silence a breakpoint without
losing its condition/trace_expr and having to retype them) or delete the field and the marker.

**Acceptance criteria**
- [ ] Either a disabled breakpoint stops firing while staying listed, or the field and `✗` are gone
- [ ] If implemented: disabling clears the JDWP request but keeps the definition, and re-enabling re-arms
      it at the same location
- [ ] No dead state left behind either way

**Blocked by**
None.

### TRACE-4 — `get_traces` can't be filtered  · P2 · S

**What to build**
Three kinds of stop point now share one 500-entry ring buffer (`handle_get_traces`,
`handlers.rs:1077`), and the only controls are `limit` and `clear`. Reading "the throws from `exc_4`"
means eyeballing everything.

Add filtering by stop-point id and/or class, and consider a `since` (sequence number) so a poller can ask
for what's new — the `seq` is already recorded for exactly this kind of use.

**Acceptance criteria**
- [ ] `debug.get_traces {bp_id}` and/or `{class_filter}` narrows the output
- [ ] `since` returns only records newer than a given seq, so polling doesn't re-read everything
- [ ] The header still says how many exist versus how many are shown

**Blocked by**
None.

### SETF-2 — `set_value` writes literals only  · P2 · M

**What to build**
`literal_to_value` refuses `ArgLit::Expr` (`handlers.rs:4523`), so a write can only be a literal. You
cannot copy a live value — `this.cfg = other.cfg`, `reserva.cliente = clienteValido` — which is how you
inject a known-good object to prove a downstream failure, the same move `force_return` supports for
return values.

**Shape of the change**
Resolve the right-hand side with `resolve_expression` (a thread/frame is already available for locals and
instance fields), then validate the resolved value's runtime type against the target's declared type
using the EVAL-3 assignability check — including the interface case. A reference of the wrong type must be
refused, not written: the JVM does **not** validate this (see the EVAL-3 note).

**Acceptance criteria**
- [ ] `set_value {target:"this.a", value:"other.b"}` writes the live reference
- [ ] A type-incompatible source is refused, naming both types
- [ ] Literals keep working unchanged, including the narrowing conversions
- [ ] Probe coverage: object field ← object field, and a refused mismatch

**Blocked by**
None. Wants EVAL-3's `assignable`, which has shipped.

### SAFE-3 — no read-only mode  · P2 · S

**What to build**
`debug.evaluate` can invoke any method on the target, and `set_value` / `force_return` / `set_field_stop`
all mutate it. Nothing distinguishes "let me look" from "let me change things", so pointing this at a
production JVM means trusting every future caller — including an agent — not to call something
destructive. `deleteAll()` is a valid expression today.

**Shape of the change**
A `JDWP_READONLY=1` env (and/or a per-session flag from `attach`) that refuses invocation, writes and
`force_return`. Note the honest cost: collection expansion and `toString()` rendering *are* invocations,
so read-only necessarily means shallower output — `expand_objects` falls back to fields and ids. Say that
in the refusal rather than pretending nothing is lost.

**Acceptance criteria**
- [ ] With read-only set, `set_value`/`force_return`/method invocation are refused with a clear reason
- [ ] Reads that need no invocation still work: locals, fields, statics, arrays, `get_stack`, watchpoint
      and exception reporting
- [ ] `list_sessions` shows which sessions are read-only
- [ ] Documented as a guard against accident, **not** a security boundary — anyone who can reach the JDWP
      port can do anything anyway

**Blocked by**
None.

</details>

---

## Appendix: original 4-week roadmap — validated status (2026-07-24)

The early "object-inspection" roadmap below was validated line-by-line against the current code, with
evidence in `path:sym` form. (The original list numbered two items `6`; renumbered 1–18 here.)

Headline (final): **all 18 items are done.** The last three to land were the type cache (8/17) and the
metrics verification + example (10/14). `docs/VARIABLE_INSPECTION_PLAN.md` now records the plan against
what was actually built, including the decisions that went the other way — chiefly that object expansion
is opt-in rather than automatic, because expanding a collection invokes methods in the debuggee.

### Week 1 — Core infrastructure — ✅ complete
1. ✅ Fix `INVALID_LENGTH` — `get_frames(thread, 0, -1)` (all frames) in `handlers.rs:handle_get_stack`.
2. ✅ `StringReference.Value` — `string.rs:get_string_value`.
3. ✅ `ReferenceType.Fields` — `reftype.rs:get_fields`.
4. ✅ `ObjectReference.GetValues` — `object.rs:get_object_values`.
5. ✅ Auto-expand strings in `get_stack` — `render_value` prints string contents (`handlers.rs:handle_get_stack` renders each local).
6. ✅ **(was BLOCKER)** Expose breakpoint events — `debug.get_last_event` tool + the session's event ring buffer (a single `last_event` slot originally; see EVT-1); the event pump stores the hit thread and `get_last_event` returns `{seq, event, thread, class, method, line}` (also for steps/exceptions). `handlers.rs:handle_get_last_event`.

### Week 2 — Object inspection — ✅ complete
7. ✅ Recursive object expansion (max depth) — `expand_objects:true` walks a bounded field tree with cycle detection (`handlers.rs:render_value_deep`). Off by default, so the shallow `TypeName (id=0x…)` rendering is still what you get unless you ask.
8. ✅ Type cache — per-connection `TypeCache` (`connection.rs`) memoises each loaded type's signature, declared fields, declared methods and superclass; shared across connection clones. Values are deliberately not cached. **Measured: 48% fewer JDWP packets on a cold deep expansion, 62% on a warm one** (see `docs/VARIABLE_INSPECTION_PLAN.md`).
9. ✅ `get_stack` object expansion — opt-in via `expand_objects` (same renderer as 7). The default still passes `thread=None` on purpose, so the cheap path performs no `toString`/invocation per local.
10. ✅ `meterRegistry` verification — `roadmap_metrics_inspection_criteria` (`mcp_integration.rs`) asserts each of the plan's original success criteria. Caveat: it runs against `examples/probes/MetricsProbe.java`, a stand-in reproducing Micrometer's object *shape* (`meterRegistry.meters : Map<String, Counter>`, `Counter.id.name`, a real `AtomicInteger`) — Spring can't be a test dependency here, and the companion `java-example-for-k8s` app isn't on this box. So the **tool** is verified against the real structure; Spring's own class names, line numbers and bean lifecycle are not.

### Week 3 — Collections & polish — ✅ complete
11. ✅ Array inspection — `extra.rs:get_array_length`/`get_array_values`; `render_value` expands arrays (first 16 elements, then `… +N more`). Surfaced through `evaluate`/`get_stack`, not a dedicated tool.
12. ✅ Special handling for List/Map/Set/Optional — element-level under `expand_objects` (entries as `key → value` for maps); `toString()` remains the shallow rendering. Keyed/indexed access via subscripts (`counts["k"]`, `lines[0]`) or ordinary method calls; slicing/predicates shipped as OBJ-2.
13. ✅ Config for inspection depth/limits — `max_result_length`, `max_frames`/`include_variables`/`package_filter`, plus `expand_objects`/`max_depth`/`max_children` and the node budget.
14. ✅ Actuator-metrics example — `examples/observability-debugging.md`, rewritten around today's tools with **captured** output (it previously showed hand-written "Expected Response" blocks for a session that was never run, and objects as bare `@0x…` IDs). Same stand-in caveat as item 10, stated in the doc.

### Week 4 — Advanced navigation — ✅ complete
15. ✅ Field-path navigation (`this.meterRegistry.meters`) — `debug.evaluate` resolves `this`/local/`Class` heads then `.field` / `.method(args)` chains (`handlers.rs:resolve_expression`). Static-method calls and object arguments shipped too (EVAL-1/EVAL-2, see Shipped).
16. ✅ Collection search/filter — `[a..b]` slices and `[?predicate]` filters (`handlers.rs:apply_subscripts`), plus `[i]`/`["k"]` indexed access.
17. ✅ Performance/caching — the per-connection `TypeCache` (item 8), plus `package_filter`, single-threaded `invoke_method`, token-trimmed outputs, and the deep-render node budget. No object-*value* cache, by design.
18. ✅ Documentation & examples — README tool table, `examples/*.rs` probes, `examples/observability-debugging.md`, `docs/`. Ongoing.

### What's actually left (net of the above)

**Nothing.** Each of the follow-ups this appendix used to point at has shipped:

- ~~**OBJ-1 — recursive object expansion**~~ (items 7, 9, 12, 13), with the type cache it wanted (item 8);
  its follow-ups **OBJ-3** (one node budget per `get_stack` call, plus the frame-id staleness it uncovered)
  and **PERF-1** (container-kind caching — measured, no gain, dropped) are closed too.
- ~~**OBJ-2 — collection search/filter**~~ (item 16), and the two gaps it left as **OBJ-4**: writing through
  a subscript, and filtering a `Map` while keeping its keys.
- ~~Items 10 & 14 (`HelloController` / actuator examples)~~ — the stand-in test and the rewritten
  `examples/observability-debugging.md`, now backed by **TEST-3**: the same criteria run against a real
  Spring Boot + Micrometer app, with the differences recorded.
- ~~Overload resolution's remaining gap~~ — interface-typed parameters and boxed primitives, shipped as
  **EVAL-3**, which also removed the arity-and-kind fallback that could pass an argument no parameter
  accepted.
