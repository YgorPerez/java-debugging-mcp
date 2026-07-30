# 0022 — The heap query ships, and it reports the pause it imposed rather than refusing

## Context

DISC-10 ([#84](https://github.com/YgorPerez/java-debugging-mcp/issues/84)) starts from a gap that nothing
else in this server can close. **Every expression needs a root** — a local in a suspended frame, `this`,
or a static field — and the most valuable objects in the stack this debugger was built for have none of
them. `infotravel`'s `ApplicationSrv` is `@ApplicationScoped` with 60 mutable cache fields, and every
reference to it is a Weld proxy held by the container. `integraWS`'s `RedisProducer` keeps
`syncCommands` and `prefixExpirationMap` as *private instance* fields on another `@ApplicationScoped`
bean, behind most of 130 endpoints. `omnibees`' injected `ObjectMapper` is statically unresolvable, and
one read would settle it. The instructive contrast is `ConfigDefaultUtils`, which holds equivalent
global state in **statics** and is trivially readable today: same kind of state, opposite debuggability,
purely because of where it lives.

`ReferenceType.Instances` (set 2, command 16) answers this. It was measured before anything was built —
`docs/heap-query-measurements.md` has the wire spec, the error codes and the method — and the headline
is counter-intuitive:

| live heap | `Instances(Widget)` → 7 objects | worst application-thread tick gap |
| --- | --- | --- |
| 2,000,000 objects | 57 ms | **522 ms** |
| 20,000 objects | 4 ms | 54 ms (baseline 50) |

Both runs returned **the same 7 objects**. JDWP requires no suspend for the command and the debugger
issues none, yet the JVM holds every application thread for a full live-heap walk. **The cost tracks the
live heap, not the result.** A `WildFly` heap on the shared 8180 is multi-GB, so one call could stall
every in-flight request for seconds — the exact harm every safety default here exists to prevent,
arriving through a tool that suspends nothing.

That made this a posture question rather than an implementation gap, which is why #84 sat as
`ready-for-human` until a decision was recorded on the issue. This ADR is that decision.

## Decision

### It ships

`debug.list_instances` exists. The alternative — writing it to `.out-of-scope/` with a documented reason
— was rejected: the questions it answers have no other route, and a debugger that declines to look at a
heap it is attached to is not being safe, it is being absent. The caller is the one who knows whether the
JVM is a shared production instance or a container nobody else is hitting, and that is the same judgement
every other tool here already delegates to them at `debug.attach`.

### Nothing refuses, and there is no acknowledgement argument

Three bounds were considered and all three rejected:

- **A refusal above some heap size.** The tool would guess on the caller's behalf about a cost the caller
  is explicitly accepting, and it would be wrong in both directions — a 3 GB heap that nobody is using is
  free to walk, a 200 MB one serving live traffic is not, and the tool can see the first number and not
  the second.
- **A heap-size pre-check.** It is *itself a heap walk*. Paying the cost to decide whether to pay the cost
  is not a bound.
- **A required acknowledgement argument.** It converts a documented price into a checkbox, and a caller
  who has read the description already knows; one who has not will pass the flag anyway.

### What it owes instead is its own measured cost

The reply leads with the **held duration** it actually measured and how many walks it took:

```
🧭 3 type(s) over 3 live-heap walk(s) — HELD APPLICATION THREADS FOR ~97ms.
```

This is ADR-0010's precedent applied to a different cost. That ADR made a traced stop point report what
it *actually* spent rather than leaning on a documented estimate, because the documented figure is one
measurement on one machine and what a caller needs is what **their** call cost **now**. The same argument
holds here with more force: the price is a function of a heap size the debugger never sees.

`CONTEXT.md` already had the word — **Held duration**, "the cost a diagnostic imposed on everyone else
using a shared instance, as opposed to how long the operation took to answer" — and this is the second
tool to report one, after `debug.thread_dump`.

Two of ADR-0010's four measurement properties carry over verbatim and are worth restating because they
decide what the number means:

- **The timer wraps the heap-walking commands and nothing else.** Not name resolution, not the
  capability check, not the rendering of the handles afterwards. Charging our own round trips to "what the
  walk cost" would report the debugger's overhead as the debuggee's price.
- **The number is not a promise about the future.** It describes this call against this heap. A caller
  comparing two moments has to read both.

### `Instances` and `InstanceCounts` ship together; `ReferringObjects` does not

They share the capability bit and they share the walk, and three types in one request measured **604 ms —
about one walk** — so batching is nearly free. `class_names` is therefore a **list**, and
`InstanceCounts` is issued for the whole batch on *every* call, even when handles are wanted: it is the
only source of a true count when the handle listing is clamped, so `max_instances: 10` against 4000 live
objects still reports 4000.

`ObjectReference.ReferringObjects` does **not** ship here. "Who is holding this?" is a different question
and belongs with #101. `ClassType.NewInstance` and `ThreadReference.Stop`/`Interrupt` remain out of scope
regardless.

### Exact-type is stated, not discovered

`Instances` is **exact type**: `Widget` answers 7 with two live `SubWidget`s in the heap, not 9. On a CDI
or EJB codebase the name a caller reaches for is usually the interface or the bean class while the live
objects are `…_$$_WeldClientProxy`, so an unstated version of this semantic would produce a confident
`0 instances` about a type with hundreds of live objects.

That is `CONTEXT.md`'s **Loaded** trap in a new costume — "not loaded" about a class the debugger is
looking straight at is a wrong answer, not one of two honest readings — and it gets the same treatment:
the tool description says it in capitals, the reply repeats it under every listing including the ones
that found something, and a `0` is worded as an answer rather than rendered as an empty block.

### `canGetInstanceInfo` is decoded in the change that consults it

Bit **16** of `CapabilitiesNew`, four past where the decoder stopped. It is asked before the command, so a
JVM without it is told "this JVM cannot answer heap queries" rather than being handed `NOT_IMPLEMENTED`
(99). Bits 12-15 are read past and **not named**: a decoded bit nothing reads is the mistake `IDSizes`
was deleted for (CLEAN-1, #27), and `canUseInstanceFilters` (12) earns a name when the `InstanceOnly`
event filter lands, not before.

## Consequences

- `debug.list_instances` is dispatched with the state-inspection tools, not the DISC discovery group,
  despite taking class names. That group answers what a class *declares*, with no suspended thread and
  no cost to anyone else; this one asks what is *alive*, and stops the world to find out.
- A type whose count is 0 costs one walk (the batch count) and no second one — there are no handles to
  fetch, so the `Instances` call is skipped. The reply's walk count reflects that.
- The handles are `@0x…` and therefore weak (ADR-0021). A listing is a snapshot of the heap at one
  instant, and a handle from it can report `Vanished` later. That is the same honesty, one layer up.
- The integration test asserts the pause **against `HeapProbe`'s own tick gaps**, not against the
  debugger's own figure. A tool reporting its own cost cannot also be the evidence that the cost is real.
