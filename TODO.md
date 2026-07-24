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
Scope to `--test mcp_integration`: a bare `cargo test -- --ignored` also un-ignores jdwp-client's
illustrative ```ignore doctests, which were never meant to compile. **With no JDK every test prints
`SKIP` and passes** — so a green run on a JDK-less machine proves nothing; grep the output for `SKIP`.

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
- **Non-suspending trace breakpoints / logpoints (TRACE-1)** — `debug.set_breakpoint {…,
  trace:true, trace_expr}` captures a snapshot (location, thread, in-scope locals/args, optional
  expression) and resumes just the hit thread (EventThread policy) — never freezes the VM. Bounded
  ring buffer (cap 500), read via `debug.get_traces {limit, clear}`. Validated — a probe looping a
  method yields N snapshots with args, loop counter strictly increasing (never frozen)
  (`examples/test_trace_bp.rs`).

---

## Backlog

> **The backlog is empty** — TRACE-1, EXC-1, SETF-1, EVAL-1, EVAL-2, WATCH-1, TEST-1, OBJ-1, OBJ-2 and
> DOC-1 are all shipped; see the Shipped section above. What's left is the follow-ups noted there: a
> session-level **type cache** (appendix items 8/17), **interface-typed parameters and boxed primitives**
> in overload resolution (needs `ReferenceType.Interfaces`), and the OBJ-2 gaps (writing through a
> subscript, filtering a `Map`'s entries). None is blocking anything.

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

- ~~**OBJ-1 — recursive object expansion**~~ (items 7, 9, 12, 13) — **shipped**, including the type
  cache it wanted (item 8). Not cached: the `classify_container` verdict, which still costs a method
  lookup per rendered object — though that lookup is now itself served from the cached method list.
- ~~**OBJ-2 — collection search/filter**~~ (item 16) — **shipped**, see above. Not covered: writing
  through a subscript (`list[0] = x` would need `List.set`/array element stores — `set_value` refuses
  it rather than doing the wrong thing), filtering a `Map`'s entries (no entry-shaped result type;
  `map.values()` then filter works), and a predicate whose *left* side needs a frame local inside a
  call argument (the frame is stale after `toArray()`).
- ~~Items 10 & 14 (HelloController / actuator examples)~~ — **shipped** as
  `roadmap_metrics_inspection_criteria` + the rewritten `examples/observability-debugging.md`. The only
  thing still owed is a run against a genuine Spring Boot app, which needs the companion
  `java-example-for-k8s` (not on this machine) — the stand-in verifies the tool, not the framework.
- Field-path *method-call* richness is done (EVAL-1 static-method invocation + EVAL-2 object
  arguments). The remaining gap in overload resolution is **interface-typed parameters** and **boxed
  primitives**: `arg_type` walks only the superclass chain, so those fall through to the
  kind-compatible fallback rather than being matched precisely. Would need ReferenceType.Interfaces.
