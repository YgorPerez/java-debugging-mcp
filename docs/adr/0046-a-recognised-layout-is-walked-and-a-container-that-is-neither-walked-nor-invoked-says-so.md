# 0046 — A recognised layout is walked, and a container that is neither walked nor invoked says so

**Status:** Accepted
**Date:** 2026-08-10
**Issue:** EVAL-15 ([#179](https://github.com/YgorPerez/java-debugging-mcp/issues/179))

## Context

`debug.evaluate {expand_objects:true}` was described, in two places in its own tool description and in a
bullet of [ADR-0006](0006-object-expansion-is-opt-in.md), as reading fields and **invoking nothing**. It
did not. `render_value_deep` attempted a container read whenever a thread was supplied, and
`classify_container` duck-types by looking for `entrySet()` + `size()`, `toArray()` + `size()`, or the exact
name `java.util.Optional`. A `Map` node then cost `entrySet()`, `toArray()` on the returned set, and
`getKey()`/`getValue()` **per entry**; a `Collection` cost `toArray()`; an `Optional` cost `isPresent()` and
`get()`.

**The part that made it a defect rather than a trade** is that the structural readers already existed and
this path did not reach them. `recognise_layout` and the walks behind it —
[EVAL-10 (#92)](https://github.com/YgorPerez/java-debugging-mcp/issues/92), the same work
[ADR-0021](0021-one-thread-is-suspended-by-its-own-tool-and-invocation-is-not-what-it-unlocks.md) records —
were wired to subscripts, slices and filters only. So `expand_objects` on a plain `HashMap` field invoked
`entrySet()` to read a map this server can walk by `table[] → Node.key/value/next` without running
anything, a few hundred lines from the code that does exactly that.

Three consequences, in the order they matter:

1. **A caller who chose `expand_objects` to avoid running debuggee code got invocations anyway**, silently,
   on an event-suspended thread. `CONTEXT.md`'s **invoke-free** entry names three hazards that property
   rules out at once — fetching an unfetched association (ADR-0032), consuming a single-pass stream,
   wedging on a monitor the hit thread does not own (ADR-0036) — and all three were back for a container.
2. **On a `debug.suspend_thread` frame the deep read of a map was unavailable in the sense that matters.**
   The invocation is refused there, so the read fell back to a field walk and the caller got
   `head`/`after`/`table[]` internals. ADR-0021's measured table row is true — the entries *are* reachable
   that way — but the answer arrives in the shape of a different question and nothing said so.
3. **It cost a refused round trip per container field** on exactly the suspension mode this project
   recommends for a shared instance.

The claim was also load-bearing somewhere else, which is how it was found: DISC-15
([#160](https://github.com/YgorPerez/java-debugging-mcp/issues/160)) was closed to
`.out-of-scope/request-context-in-one-call.md` partly because reading a request's parameters by *fields*
was unavailable. That is still the verdict, and the reason is now this ADR rather than an overstatement.

## Decision

### The walk comes first, and the order is the whole fix

`render_node` tries `recognise_layout` **before** anything that invokes. A `HashMap`, `LinkedHashMap`,
`ConcurrentHashMap` or `ArrayList` node is read by `render_layout_deep`, which walks the object's own
fields. Only a container this server does not recognise reaches `render_collection_deep`, which invokes and
therefore still needs a thread suspended by an event.

**It needs no thread at all**, which is what makes this more than a cost saving: the four
`KNOWN_LAYOUTS` are readable from a bare object handle with no suspended frame anywhere — the flow that
drills into a trace snapshot after the fact.

### The route is reported in a note and never in the shape of the output

`render_layout_deep`'s rendering is deliberately byte-identical to the invoking path's, down to the
iteration order: `hash_map_entries` walks the table, which is `entrySet()`'s order, and
`linked_map_entries` walks `head`/`after`, which is the order that class exists for. A caller comparing two
replies must not have to know how each was obtained in order to compare them.

Which route was taken goes to the existing `ReadPath` notes, deduped per call, so a field tree holding
forty `HashMap`s says it once. This is EVAL-10's rule applied to a second surface, not a new one.

### A container that is neither walked nor invoked is a third verdict

`ReadPath` had two: **walked** and **fell back to invoking**. Neither describes what the deep path does
when it can do neither — it prints the object's internals, which is *true about a different question*.
That is the shape of answer this repo distrusts most: a `TreeMap` rendered as `root`/`left`/`right` is not
wrong, it is just not the entries the caller asked for. `internals_only` says so, and names the remedy (a
stop point on the code you want to ask about).

The commonest route to it is a `debug.suspend_thread` frame, where the invocation is **refused** rather
than absent. Reporting it is what turns ADR-0021's row from a true statement into a legible one.

## Rejected alternatives

**Walk an unrecognised map generically** — a `TreeMap` by `root → Entry.left/right/key/value`, Undertow's
`HeaderMap` by its own arrays. Reachable, and refused for the reason `KNOWN_LAYOUTS` exists: recognition is
by the runtime type's **exact signature**, never a superclass or an interface, because a `HashMap` subclass
may keep its entries elsewhere entirely and a `Collections.synchronizedMap` wrapper holds a delegate rather
than a table. A walk that guessed would return a confident wrong answer, which is worse than an invocation
that is refused and says why.

**Keep invoking and only fix the description.** Cheaper, and it was the state this ADR replaces. It leaves
the capability present and unreachable from the one suspension mode a shared instance can afford.

**Label the route in the rendered line** (`params(3 entries, walked) { … }`) rather than in a note. It
would make two replies about the same map differ in their body according to how each was read, which is
the thing the byte-identical rule exists to prevent.

**Gate the structural path on a thread being supplied**, to keep every thread-less render a field walk as
before. Considered because it is the smaller diff, and rejected: a structural read does not use the thread,
so requiring one would be a rule with no mechanism behind it — the kind of incoherence that gets
"simplified" away later by someone who cannot see why it was there.

## Consequences

- A deep render is invoke-free for the four layouts and only for them, which is now what `CONTEXT.md`,
  `docs/tools.md` and both tool descriptions say.
- `get_stack {expand_objects:true}` carries the notes too, once per call. They trail the reply **after**
  the node-budget notice, so `get_stack_node_budget_bounds_the_whole_call` anchors on "no frame was
  expanded after the cap" rather than on the end of the string.
- `FieldIds` gained a one-entry known-type hint. Without it a walk re-read `ObjectReference.ReferenceType`
  for the object its caller had resolved one packet earlier — byte-for-byte the duplicate PERF-2 (#129)
  removed, and `a_rendered_object_is_asked_for_its_type_once` caught it immediately.
- Node cost per container drops, so a shared node budget stretches further. That is why the budget test's
  anchor moved rather than its cap.
- No new JDWP command: every walk uses `ObjectReference.GetValues`, `ArrayReference.GetValues`/`Length` and
  `ReferenceType.Fields`, all already classified as `Read` in `WIRE_COMMANDS` (ADR-0001, SAFE-12).
