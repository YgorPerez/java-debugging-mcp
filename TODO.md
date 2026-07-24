# java-debugging-mcp — improvement backlog

Tracked as independently-grabbable vertical slices (per the `/to-issues` convention): each item is a
complete end-to-end capability — JDWP primitive(s) in `jdwp-client` + wiring/tool in `mcp-server` +
a validation against a live probe — not a horizontal layer. A fresh session can grab any unblocked
item and finish it.

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
- **Exception breakpoints (EXC-1)** — `debug.set_exception_breakpoint {class_pattern, caught,
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
- **Field watchpoints (WATCH-1)** — `debug.set_watchpoint {class_name, field_name, modify, access}`
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
  `debug.set_exception_breakpoint {…, trace:true}` and `debug.set_watchpoint {…, trace:true}` arm with
  `EventThread`, snapshot the hit, and resume that thread — so the two tools you most want on the
  shared 8180 (silent catches; "who mutates this?") no longer freeze other people's requests. A traced
  throw records the exception type + caught/catch location; a traced write records the **old → new**
  pair, captured at hit time because the old value is only readable before the pending store commits.
  The hit path is one `find_traced_request` lookup across `breakpoints` / `exception_requests` /
  `watchpoints` — deliberately three small scans rather than a fourth map keyed by request id, which
  would be a second source of truth that could outlive the entry it points at. `get_last_event`'s
  exception/field describers are now shared with the trace capture, so a traced hit reports exactly
  what a suspending one would. `list_breakpoints` marks traced entries, and both tool descriptions now
  say that the default suspends everything.
  Validated by `traced_exception_breakpoints_…` and `traced_watchpoints_…` in `mcp_integration.rs`
  (+ `examples/probes/ExcProbe.java`, a throw-and-swallow loop): each asserts the hits land in
  `get_traces` **and** that the probe's own tick line keeps advancing — the debugger reports success
  either way, so only the debuggee's output proves nothing was left suspended.
  The `jdwp-trace` skill in the sibling repo is updated to match: Rule 0 now covers all three kinds,
  site 2's step 2 is traced, and the one thing a trace genuinely can't give you (the calling stack) is
  stated once, where the suspension discipline lives.
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
- **Non-suspending trace breakpoints / logpoints (TRACE-1)** — `debug.set_breakpoint {…,
  trace:true, trace_expr}` captures a snapshot (location, thread, in-scope locals/args, optional
  expression) and resumes just the hit thread (EventThread policy) — never freezes the VM. Bounded
  ring buffer (cap 500), read via `debug.get_traces {limit, clear}`. Validated — a probe looping a
  method yields N snapshots with args, loop counter strictly increasing (never frozen)
  (`examples/test_trace_bp.rs`).

---

## Backlog

**Empty.** Everything filed here has shipped — the original ten items (TRACE-1, EXC-1, SETF-1, EVAL-1,
EVAL-2, WATCH-1, TEST-1, OBJ-1, OBJ-2, DOC-1), all 18 appendix items, and all ten from the
post-completion review (TRACE-2, OBJ-3, EVT-1, SESS-1, EVAL-3, OBJ-4, PERF-1, TEST-2, DOC-2, TEST-3).
PERF-1 closed as *measured, no gain*, which is a result rather than a build.

New work goes here as a vertical slice, with the same shape as the entries above: what to build, the
shape of the change, acceptance criteria you can check, and what blocks it. The validation pattern is at
the top of this file.

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
6. ✅ **(was BLOCKER)** Expose breakpoint events — `debug.get_last_event` tool + `session.last_event`; the event pump stores the hit thread and `get_last_event` returns `{event, thread, class, method, line}` (also for steps/exceptions). `handlers.rs:handle_get_last_event`.

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
