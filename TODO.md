# java-debugging-mcp — improvement backlog

Tracked as independently-grabbable vertical slices (per the `/to-issues` convention): each item is a
complete end-to-end capability — JDWP primitive(s) in `jdwp-client` + wiring/tool in `mcp-server` +
a validation against a live probe — not a horizontal layer. A fresh session can grab any unblocked
item and finish it.

## How to validate anything here (the house pattern)

There is no automated integration harness for the MCP layer yet (see TEST-1), so features are proven
with a throwaway example that drives `jdwp-client` against a purpose-built probe JVM:

1. Write a tiny Java probe with the shape you need; compile with the JBR javac at
   `/snap/intellij-idea-ultimate/*/jbr/bin/javac` (no system JDK on this box, JRE only).
2. Launch it with `-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:<port>`
   (dt_socket `server=y` accepts ONE connection then stops listening — use a fresh port per run).
3. Add an `examples/test_*.rs` (register it in `jdwp-client/Cargo.toml`) that exercises the new
   primitives and asserts the outcome; run with `cargo run --release --example …`.

See `examples/test_static_field.rs` and `examples/test_deferred_bp.rs` for the two worked patterns.

Note: the `mcp__jdwp__` tools in a running Claude Code session hold the OLD binary — a rebuild
(`cargo build --release`) is only picked up after a Claude Code restart. That's why validation goes
through library examples, not the live tools, within a session.

---

## ✅ Shipped (context)

- **Static-field reads in `debug.evaluate`** — `ConfigDefaultUtils.dsUrlMotor`, with or without a
  suspended frame. Primitives `get_reference_values` (ReferenceType.GetValues) + `all_classes`.
  Validated (`examples/test_static_field.rs`).
- **Deferred / class-prepare breakpoints** — a breakpoint on a not-yet-loaded class auto-arms on
  load. Primitives: ClassPrepare wire-decode, `set_class_prepare`/`clear_class_prepare`,
  `resume_thread`. Validated end-to-end (`examples/test_deferred_bp.rs`).
- **`debug.force_return`** — force the current method to return a value, skipping its body.
  Primitive `force_early_return` (ThreadReference.ForceEarlyReturn). ⚠️ compile-verified only — a
  runtime example is still owed (see TEST-1).
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

> **TRACE-1, EXC-1, SETF-1 (all P1) are done** — see the Shipped section above.

### EVAL-1 — Static-method invocation in `debug.evaluate`  · P2 · M

**What to build**
Let `evaluate` call static methods (`ConfigDefaultUtils.getX()`, `SomeSrv.helper(a)`), not just read
static fields. `resolve_static_head` currently reads fields only. Add ClassType.InvokeMethod and route
a trailing `(...)` on a class-prefixed head through it.

**Acceptance criteria**
- [ ] `debug.evaluate {expression:"SomeClass.staticMethod(1)"}` returns the result
- [ ] Works on a resolved class from FQN or bare-name scan; needs a suspended thread (document it)
- [ ] Probe with a static method returning a value, invoked via evaluate, matches the real return

**Blocked by**
None. Pairs naturally with EVAL-2.

### EVAL-2 — Object arguments in method calls  · P2 · M

**What to build**
`evaluate` method-call args are primitive/string/null literals only (`arglit`). Allow passing an
existing object — a local, `this`, or a sub-expression — as an argument (`foo.matches(reserva)`,
`svc.handle(this)`). Also unlocks richer conditional breakpoints.

**Acceptance criteria**
- [ ] An argument that is an identifier/sub-expression resolves to a value and is passed by reference
- [ ] Method overload resolution still picks the right method with object args
- [ ] Probe: call a method taking an object arg, using a local as the arg, and assert the result

**Blocked by**
None.

### WATCH-1 — Field watchpoints (modification)  · P2 · M

**What to build**
Break when a field is modified (FIELD_MODIFICATION event) — "who mutates this?" e.g. `it_b2c.empresa_id`.
Optionally FIELD_ACCESS too. Needs the field event decode + EventRequest with a FieldOnly modifier +
a `debug.set_watchpoint {class, field, access|modify}` tool.

**Acceptance criteria**
- [ ] Field modification event decoded (thread, field, old/new value, location)
- [ ] `set_field_watch(ref_type, field_id, modify/access)` + clear in `jdwp-client`
- [ ] `debug.set_watchpoint` tool; hit reports the mutating location + old→new value
- [ ] Probe that mutates a watched field stops with the correct old/new values

**Blocked by**
None. (Shares the event-parse + EventRequest.Set pattern with EXC-1.)

### TEST-1 — MCP-handler integration tests + force_return runtime example  · P2 · M

**What to build**
The two examples validate the `jdwp-client` layer, but the `mcp-server` glue (event pump arming
deferred breakpoints, `handle_set_breakpoint` deferred path + race re-check, `handle_force_return`)
has no automated coverage. Build a small harness that spins up the MCP server against a probe JVM and
drives it over JSON-RPC, plus a runtime example proving `force_early_return` actually changes a
method's returned value.

**Acceptance criteria**
- [ ] A test starts the server, attaches to a probe, and asserts a deferred breakpoint arms + fires through the real handlers
- [ ] `force_return` runtime example: break in a method, force a value, continue, observe the caller receive the forced value
- [ ] Runs from `cargo test` (or a documented script) without manual steps beyond launching the probe

**Blocked by**
None.

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
