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

Priority key: **P1** = highest payoff for the infotravel/integraWS investigations (shared 8180 +
silent-failure debugging); **P2** = solid follow-ups; effort is rough (S/M).

> **TRACE-1, EXC-1, SETF-1 (all P1) and EVAL-1, EVAL-2, WATCH-1, TEST-1 are done** — see Shipped
> above. Only DOC-1 and the two OBJ-* items remain.

### DOC-1 — `jdwp-trace` skill / silent-catch playbook (infotravel-dev-toolkit)  · P2 · S

**What to build**
Lives in the sibling `infotravel-dev-toolkit` repo, but tracked here because it depends on TRACE-1 +
EXC-1. A skill (or recipes folded into run-infotravel) that pairs logpoints + exception breakpoints
with the known silent-catch sites, as a ready playbook: "OTP/email/save silently fails → exception-
break these classes / trace these methods."

**Acceptance criteria**
- [ ] Names the concrete silent-catch sites (IntegraSrv.post non-200, EnviaEmailSrv:162, ErrorException swallows)
- [ ] Gives a copy-paste trace/exception-breakpoint recipe for each
- [ ] Cross-linked from run-infotravel § 3 and ask-infotravel

**Blocked by**
~~TRACE-1, EXC-1~~ — both shipped; DOC-1 is now unblocked.

---

## Appendix: original 4-week roadmap — validated status (2026-07-24)

The early "object-inspection" roadmap below was validated line-by-line against the current code.
Legend: ✅ done · 🟡 partial · ⬜ not started. Evidence in `path:sym` form. (The original list
numbered two items `6`; renumbered 1–17 here.)

Headline: **Week 1 is fully done, and the Week 4 headline — field-path navigation — already shipped
via `debug.evaluate`.** What's genuinely missing is *automatic recursive object expansion* and
*collection-aware inspection*; today you drill into objects/collections manually with `evaluate`.

### Week 1 — Core infrastructure — ✅ complete
1. ✅ Fix `INVALID_LENGTH` — `get_frames(thread, 0, -1)` (all frames) in `handlers.rs:handle_get_stack`.
2. ✅ `StringReference.Value` — `string.rs:get_string_value`.
3. ✅ `ReferenceType.Fields` — `reftype.rs:get_fields`.
4. ✅ `ObjectReference.GetValues` — `object.rs:get_object_values`.
5. ✅ Auto-expand strings in `get_stack` — `render_value` prints string contents (`handlers.rs:handle_get_stack` renders each local).
6. ✅ **(was BLOCKER)** Expose breakpoint events — `debug.get_last_event` tool + `session.last_event`; the event pump stores the hit thread and `get_last_event` returns `{event, thread, class, method, line}` (also for steps/exceptions). `handlers.rs:handle_get_last_event`.

### Week 2 — Object inspection — 🟡 partial
7. ⬜ Recursive object expansion (max depth) — not implemented. Objects render as `TypeName (id=0x…)` (or `TypeName "toString()"` when a thread is available in `evaluate`); there is no field-tree walk and no depth bound (`render_value`). Drilling is manual via `evaluate` `.field` chains.
8. 🟡 Type cache — only a per-call class-name cache in `get_stack` (`class_names` map) plus `package_filter` to skip framework frames; no persistent/session type or object cache.
9. 🟡 `get_stack` auto-expands objects — expands **strings + arrays** only; objects show type + id (rendered with `thread=None` on purpose, to keep `get_stack` cheap — no `toString`/invocation). Full object auto-expansion is 7.
10. ⬜ HelloController `meterRegistry` verification — no such automated test; `examples/observability-debugging.md` is the closest (a manual write-up).

### Week 3 — Collections & polish — 🟡 partial
11. ✅ Array inspection — `extra.rs:get_array_length`/`get_array_values`; `render_value` expands arrays (first 16 elements, then `… +N more`). Surfaced through `evaluate`/`get_stack`, not a dedicated tool.
12. 🟡 Special handling for List/Map/Set/Optional — no structural/element-level expansion; they render via `toString()` in `evaluate` (readable) but not in `get_stack`. No keyed/indexed element access.
13. 🟡 Config for inspection depth/limits — have `max_result_length` (evaluate), `max_frames`/`include_variables`/`package_filter` (get_stack), and a hardcoded 16-element array cap. No **depth** knob (there is no recursion to bound yet).
14. ⬜ Actuator-metrics debugging example — not present as a runnable example.

### Week 4 — Advanced navigation — 🟡 partial (headline done)
15. ✅ Field-path navigation (`this.meterRegistry.meters`) — `debug.evaluate` resolves `this`/local/`Class` heads then `.field` / `.method(args)` chains (`handlers.rs:resolve_expression`). Static-method calls and object arguments shipped too (EVAL-1/EVAL-2, see Shipped).
16. ⬜ Collection search/filter — not implemented.
17. 🟡 Performance/caching — class-name cache, `package_filter`, single-threaded `invoke_method`, and token-trimmed outputs exist; no general type/object-id cache.
18. ✅ Documentation & examples — README tool table, `examples/*.rs` probes, `examples/observability-debugging.md`, `docs/`. Ongoing.

### What's actually left (net of the above)

- **OBJ-1 — recursive object expansion** (items 7, 9, 12, 13): an opt-in `debug.get_stack {expand_objects:true, max_depth}` / `debug.evaluate` deep mode that walks instance fields to a bounded depth with a per-node cap, cycle detection, and collection-aware rendering (List/Map/Set/Optional element-level, not just `toString`). Needs the depth/breadth config knobs (13) and a real type cache (8) to stay cheap. **New item — not yet in the backlog above.**
- **OBJ-2 — collection search/filter** (item 16): filter/slice large collections during inspection (e.g. `list[0..10]`, `map.get("k")` already works via EVAL; a predicate filter does not). Depends on OBJ-1. **New item.**
- Items 10 & 14 (HelloController / actuator examples): the integration harness they needed now exists
  (TEST-1), so these are just cases to add to `mcp_integration.rs` plus a write-up under **DOC-1**.
- Field-path *method-call* richness is done (EVAL-1 static-method invocation + EVAL-2 object
  arguments). The remaining gap in overload resolution is **interface-typed parameters** and **boxed
  primitives**: `arg_type` walks only the superclass chain, so those fall through to the
  kind-compatible fallback rather than being matched precisely. Would need ReferenceType.Interfaces.
