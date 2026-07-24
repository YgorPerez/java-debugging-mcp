# Variable Inspection — plan, and what it became

**Status: delivered.** This started as a 4-week plan to make the debugger useful for inspecting live
variables. Every phase shipped; the document is kept as the record of what was planned, what was built,
and the handful of decisions worth remembering.

For the current tool list see `README.md`. For where each roadmap item landed in the code, see the
appendix of `TODO.md`.

## The goal

**Core use case**: *"Why isn't my custom metric showing up in `/actuator/metrics`?"*

To answer that, a user needed to set a breakpoint where a metric is registered, find the right thread
and frame, see real values rather than object IDs, inspect an object's fields, and drill into
collections and nested objects.

All six work. `examples/observability-debugging.md` walks the use case end to end with captured output,
and `roadmap_metrics_inspection_criteria` in `mcp-server/tests/mcp_integration.rs` asserts the original
success criteria as an automated test.

## What was built

| Planned | Shipped as |
|---|---|
| String values, not object IDs | `render_value` — strings, arrays and boxed wrappers render as their contents |
| Object field access | `debug.evaluate {expand_objects:true}` — a bounded field tree, own + inherited |
| Collection inspection | element-level `List`/`Set`/`Map`/`Optional`; maps as `key → value` |
| Which thread hit the breakpoint (the Week-1 **blocker**) | `debug.get_last_event` — a machine-readable `[event]` line naming the thread and location |
| Type information cache | per-connection `TypeCache` (`jdwp-client/src/connection.rs`) |
| Field-path navigation (`this.meterRegistry.meters`) | `debug.evaluate` resolves local/`this`/`Class` heads then `.field` / `.method(args)` chains |
| Collection search | `[?predicate]` filters, `[a..b]` slices, `[i]` / `["key"]` subscripts |
| Expression evaluation (deferred as "Phase 3, future") | shipped — including static-method invocation and object arguments |

Beyond the plan: conditional breakpoints, non-suspending logpoints, exception breakpoints, field
watchpoints, live field writes, `force_return`, multiple concurrent sessions, and a safety watchdog.

## Decisions worth remembering

The plan's Open Questions, as answered by the implementation.

**How deep should objects auto-expand?** They don't — expansion is **opt-in**. The plan assumed
auto-expansion at `max_depth=2`, but expanding a collection means *invoking methods in the debuggee*
(`toArray`, `entrySet`), which needs a suspended thread and has side effects. Doing that for every local
of every frame by default would make `get_stack` slow and side-effecting. So the default renders objects
as `Type (id=0x…)` with no invocation at all, and `expand_objects:true` asks for the tree. `max_depth`
does default to 2 once you ask.

**How to handle circular references?** Cycle detection is **path-based**: an object already on the
current path renders as `↩ Type (id=…, cycle)`. Path-based rather than globally-seen on purpose — a
value reachable twice by different routes is worth printing twice when you're inspecting, whereas a true
cycle must not recurse.

**How to handle large collections?** `max_children` (default 16) bounds fields per object and elements
per collection, and the output states what it truncated. A total **node budget** bounds the whole call,
so a shallow-but-bushy graph can't blow up either — 400 for one `debug.evaluate`, 1000 shared across an
entire `debug.get_stack` (it expands many values, not one; per-value budgets would multiply). For finding rather than browsing, filter:
`[?pred]` scans up to 1000 elements and reports `N of M matched`, so an empty result is distinguishable
from an unscanned one.

**Should object field values be cached?** No — as the plan proposed. The `TypeCache` holds only a type's
*shape* (signature, declared fields, declared methods, superclass), which is immutable for a loaded
type. Values change as the program runs, so a cached value would be a lie.

**How to present nested data?** Indented braces rather than the planned `├─ └─` tree characters — they
survive terminal and JSON round-trips better, and match how the stack output already reads.

## The type cache, measured

Object inspection asks the same questions repeatedly: walking a superclass chain to find one field, or
scoring method overloads, re-reads the same field and method lists for every object of a type. Expanding
a 20-element collection asked the JVM for the element type's fields 20 times.

Counting JDWP packets for one deep expansion of the `DeepProbe` graph (`max_depth:3, max_children:30`):

| | cold expansion | a second identical expansion |
|---|---|---|
| without cache | 421 packets | 414 more |
| with cache | 218 packets | 159 more |

**48% fewer packets cold, 62% warm.** Cold still improves because the same types recur *within* a single
expansion. Wall-clock barely moves over loopback, where a round trip is sub-millisecond — the win is a
remote JVM (e.g. `kubectl port-forward`), where 200 fewer round trips is the difference.

Staleness is bounded by design: the cache belongs to the connection, so reattaching after a redeploy
gets a fresh one. A class unload or an external `RedefineClasses` could serve stale shape data; both are
documented at `TypeCache`.

## Testing

The plan called for unit tests per JDWP command, integration tests against a real app, and validating
the example step by step. What exists:

- **Unit tests** for the pure logic — schema generation, the type cache, tool registration.
- **Integration tests** (`mcp-server/tests/mcp_integration.rs`) driving the real `jdwp-mcp` binary over
  JSON-RPC against probe JVMs the harness compiles, launches and reaps itself. Run with
  `scripts/integration-test.sh`. Eleven tests cover expression resolution, watchpoints, deferred
  breakpoints, `force_return`, deep expansion and its node budget, collection subscripts, the event
  buffer, non-suspending traces, and the roadmap criteria above.
- **The example is validated** by the roadmap-criteria test, and its output blocks are captured from a
  real run rather than written by hand.

Mock-JDWP-response unit tests were **not** built. Driving a real JVM caught things a mock could not
have: a modifier-kind mix-up that came back as a bare `INTERNAL` naming nothing, frame IDs silently
invalidated by method invocation, and an ill-typed invoke that **SIGSEGV'd the debuggee**. A mock would
have agreed with all three.

## Non-goals (unchanged)

Full debugger UI · hot code reload · code modification beyond `set_value`/`force_return` · performance
profiling · memory-leak detection.

## Resources

- [JDWP Specification](https://docs.oracle.com/javase/8/docs/technotes/guides/jpda/jdwp-spec.html)
- [JDI Documentation](https://docs.oracle.com/javase/8/docs/jdk/api/jpda/jdi/) — the reference implementation
