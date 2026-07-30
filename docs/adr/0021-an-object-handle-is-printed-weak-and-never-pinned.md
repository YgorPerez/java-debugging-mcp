# 0021 — An object handle is the spelling every reply prints, and it stays weak

## Context

TRACE-10 ([#85](https://github.com/YgorPerez/java-debugging-mcp/issues/85)) is two halves of one problem: a
trace snapshot tells you something happened and then gives you nowhere to go. A `TraceRecord` held
**rendered strings**, so a snapshot naming `WSReserva` could not be drilled into afterwards, and the
expression head resolver accepted a local, `this`, or a class name and nothing else. The most valuable
objects a snapshot sees have none of those: a request that crossed a thread boundary is not a local of any
suspended frame, and by the time you read the snapshot there is no suspended frame at all.

Two decisions had to be taken rather than fallen into. **What the head syntax is**, since the issue said
"pick and document it"; and **whether a retained id is kept alive**, since JDWP offers
`ObjectReference.DisableCollection` / `EnableCollection` (set 9, commands 7 and 8) and the constants have
been sitting unused in `commands.rs` since the beginning.

## Decision

### The handle is `@0x<hex>`, and it is what the reply already prints

An object handle is an expression **head** — first segment only — spelled `@0x1f4c`. The same string is
what a rendered object now ends in: `render_object`'s shallow form changed from `com.example.Order
(id=0x1f4c)` to `com.example.Order @0x1f4c`, and the deep render, the array and field-block headers, the
cycle marker and the trace snapshot's object-valued entries all use it.

That identity is the decision, not a formatting preference. `CONTEXT.md` records the rule under **Loaded**,
learned from SIG-1 (#46): *a name this tool shows is a name it accepts*. #46 was a caller pasting a lambda's
real name back in and being told it might not be loaded, because the tool spelled its own output
differently from what it would take. A handle printed in one form and accepted in another would reproduce
that exactly, on a value instead of a class.

Where the rendering does not carry the id — a String renders as its contents, an array as its elements, a
boxed primitive as the number it holds — a snapshot appends the handle beside the value. `TracedValue`
therefore carries the id **beside** the rendered text rather than leaving it inside it, which is the
structural half of the issue's "carry ids as well as text".

The `@` is required and the digits are hex. A bare `0x1f4c` would be a plausible *number* in an argument
position, and a decimal id would not match the form it was copied from.

**This is deliberately the same spelling ADR-0019's classloader selector uses**, and the two compose
rather than collide. BP-5 (#79) pins which copy of a duplicated class a read resolves against by
*suffixing* the class name with the loader's own objectID — `com.example.Utils@0x7f3a1c` — chosen as a
suffix precisely so it travels through `trace_expr`, where there is no schema to extend. Both are
therefore "`@0x` + a JDWP objectID, in hex, copy-pasted out of a reply", which is one rule rather than
two that happen to rhyme, and **position** disambiguates them without ambiguity: a handle is the *whole*
first segment, so the parser treats a token that **starts** with `@` as a handle and leaves
`Utils@0x7f3a1c` — where the `@` is interior — entirely to the class-name path. A caller who has learned
one has learned the other; had this issue picked `#1f4c` or `obj:1f4c` there would be two conventions for
"an object id you can paste back in", which is exactly the kind of drift `CONTEXT.md` exists to stop.

### The id stays weak. Nothing is pinned

`DisableCollection` is **rejected**, and the issue named it as the thing to consider.

Pinning would work: the object could not be collected, so the handle would always dereference. That is
precisely the problem. This debugger is built to be pointed at a **shared** application server, and every
safety default here exists so that observing it does not degrade it. An object the debugger has pinned is
an object the collector cannot reclaim, and the leak is not bounded by anything a caller can see: a trace
buffer holds up to `MAX_TRACES` snapshots, each snapshot can carry several object-valued entries, and each
pinned object retains everything it references. On the fan-out this issue is about — 57 anonymous
`Callable`s, each capturing a session and a request — pinning the captures of a busy trace would hold a
graph of live request state that the JVM was finished with. A debugger that quietly becomes the reason a
production heap grows is worse than one that sometimes cannot answer.

The issue's own framing anticipated this: *"if you do, be explicit about the leak risk … pinning must be
bounded and released with the record."* Bounded and released is achievable — and it converts a simple
mechanism into one with a lifetime, a release path, and a failure mode (a released pin on an id a caller
still holds is indistinguishable from a collection anyway). The cost is real, the benefit is that a handle
works slightly more often, and the case it would rescue is the one `CONTEXT.md` already calls **ordinary**.

So a handle is a **weak** reference and the tool says so, in the tool description, in the argument
documentation, and in the reply when one fails.

### A failed handle is reported as *vanished*, with the two readings distinguished

`CONTEXT.md` defines **Vanished** for exactly this — "listed by the JVM and already gone by the time the
debugger asked" — and notes it is the ordinary case on a pool that retires workers. A handle whose object
is gone is that, not an error.

The trap is that JDWP answers `INVALID_OBJECT` (20) both for an object that was collected and for an id
that was never valid, so the naive implementation cannot tell a garbage-collected request from a typo. So
liveness is asked **before** the read, with `ObjectReference.IsCollected` (set 9, command 9):

- `IsCollected` → `true`: the debuggee still remembers the id and says the object is gone. Certain.
- `IsCollected` → `INVALID_OBJECT`: the debuggee has no record of the id at all — collected long enough ago
  that the mapping went too, **or** never issued by this JVM. Both readings are offered, because neither can
  be ruled out, which is the same obligation the **Loaded** entry imposes.

The reply names the handle, says the id is weak, says nothing pins it, and says what to do next. It never
shows the JDWP error code as the answer.

## Rejected alternatives

**A `handle:` argument on `debug.evaluate` instead of a head syntax.** It would work for `debug.evaluate`
and nowhere else. A `condition`, a `trace_expr` and a filter predicate are *strings* with no schema to
extend, so an argument-shaped handle would be unusable in exactly the places a trace snapshot leads to. The
head form composes because it is part of the expression grammar.

**Reusing the `(id=0x…)` spelling as the accepted form.** Parsing `(id=0x…)` as a head means the grammar
contains a form nobody would write by hand, and the parentheses collide with a call. Changing the render
was the cheaper half.

**Keeping the old render and adding the handle beside it.** Every plain object would then print its id
twice — `Order (id=0x1f4c) @0x1f4c` — which is what a reader would have to learn to ignore. The formatter
does test the rendered text before appending, but only so the three renderings that genuinely lack an id
get one; it is not a licence to print both spellings.

**Auto-retrying a vanished handle against a newer snapshot of the same type.** Guessing which object the
caller meant, from a type name, after the one they asked about is gone. The reading is the answer.

## Consequences

- `ObjectReference.IsCollected` is implemented in `jdwp-client`; `DisableCollection` and
  `EnableCollection` remain unimplemented constants, and the `is_collected` doc comment says why so the
  next reader does not treat their absence as an oversight.
- `TraceRecord.args` becomes `Vec<TracedValue>` and gains `captured`, the anonymous-inner-class section
  from the other half of #85. Both carry ids, so both are drillable.
- The rendered form of an object changed in **every** reply that renders one — `debug.evaluate`,
  `debug.get_stack`, `debug.get_traces`, `debug.get_last_event`. That is caller-visible and belongs in the
  release notes, per `docs/toolkit-contract.md`; downstream prose quoting `Type (id=0x…)` is now stale.
- ADR-0006's shallow-render example is updated to match, since it is the ADR that explains why the shallow
  form exists at all.
