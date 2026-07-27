# Method-Entry Events

This debugger does not arm `METHOD_ENTRY` (JDWP event kind 40), and `EventKind::MethodEntry` is
deliberately absent from the client's event enum. The constant is still named in the event-kind table
in `jdwp-client`, as a spec reference — not as an oversight waiting to be wired up.

## Why this is out of scope

**It is the noisiest event in JDWP.** A `METHOD_ENTRY` request is filtered by *class*, not by method.
`ClassMatch` on `com.example.OrderService` fires on every entry to every method of every matching
class — getters, `equals`, `hashCode`, everything the class does — and each hit is a packet on the one
connection this server multiplexes over. On the shared 8180 that is not a diagnostic, it is a load
test. The measured trace ceiling of ~720 hits/s (TRACE-6, #22) is the budget the whole session shares,
and method entry is the fastest way to spend it on nothing.

**The question it would answer is already answered, more cheaply.** People reach for method entry to
ask *what calls this?* Since TRACE-5 (#14) a traced stop point captures the caller chain above each
hit, so a `trace:true` line breakpoint on the method's first line answers the same question — at one
site instead of every method on the class, without suspending, and with a `trace_max_hits` budget
bounding it. That is a strictly better shape for the same need.

**A decoded event nothing can arm is a lie about capability.** METH-1 (#16) found `MethodEntry` and
`MethodExit` both fully decoded and neither armable. `MethodExit` got a tool — `debug.set_method_exit_stop`,
where the return value is information available nowhere else, and where trace mode is the default
precisely because a suspending method exit on a hot method freezes a VM fastest. `MethodEntry` had no
comparable payload: entry tells you a call happened, which the caller chain already tells you, with
context. The variant was removed rather than left implying a feature that was not there.

```rust
// jdwp-client, event-kind table — the constant stays, the capability does not:
//
// - `METHOD_ENTRY` (40) is named but intentionally NOT wired up. With a `ClassMatch` it fires on every
//   method of every matching class — the noisiest event in JDWP — and "what calls this?" is answered
//   far more cheaply by a traced breakpoint's caller chain (TRACE-5). `EventKind::MethodEntry` was
//   removed for that reason (METH-1); this constant is a spec reference, not an oversight to fix.
```

## What would reopen this

A need that a traced breakpoint's caller chain genuinely cannot serve — the plausible one being
*"which methods of this class get called at all, under this workload?"*, a coverage-shaped question
rather than a debugging-shaped one, where the class-wide firing is the point rather than the cost.
That would still need the volume answered before it could ship: a method-name filter applied inside
the debugger does not help, because the packet has already crossed the wire by then.

## Prior requests

- #16 — METH-1, which removed the decoded-but-unarmable variant and settled this
- Raised again 2026-07-26 while comparing tool surfaces against
  [`d4n-sec/jdb-mcp`](https://github.com/d4n-sec/jdb-mcp), which exposes `debug_set_method_entry`.
  Not filed as an issue — the decision above still holds.
