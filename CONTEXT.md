# Java Debugging over JDWP

A native JDWP debugger exposed as MCP tools, built to be pointed at a **shared** application server that
other people are using. Almost every term below exists because the debugger must be able to observe that
server without stopping it, and must be honest about it when it does.

## Language

### The target

**Debuggee**:
The JVM being debugged. Referred to as "the VM" when the subject is its suspension state rather than the
process.
_Avoid_: target, remote, server (the last belongs to the MCP server, which is this program)

**The shared 8180**:
The `WildFly` instance several people use at once. Not an environment name — the constraint that decides
every safety default, because freezing it stalls other people's requests.

### Stop points

**Stop point**:
Anything armed in the debuggee that reports when execution reaches it. The umbrella over all four kinds.
_Avoid_: breakpoint (when you mean any kind rather than a line breakpoint specifically)

The **tool names follow this**, as of VOCAB-1 (#20): `debug.set_line_stop`, `debug.set_exception_stop`,
`debug.set_field_stop`, `debug.set_method_exit_stop`, and `debug.clear_stop_point` /
`debug.list_stop_points` / `debug.toggle_stop_point` across all four. Before that, `breakpoint` named
three different scopes depending on where you read it — one source location in `set_breakpoint`, two
things that were not source locations in `set_exception_breakpoint` and `set_method_breakpoint`, all four
kinds in `clear_breakpoint` / `list_breakpoints` / `toggle_breakpoint`, and `set_watchpoint` was a stop
point that the word did not cover at all. The renames were taken while nothing scripted against them yet;
the window for doing it cheaply does not reopen.

Two things deliberately did **not** change, so this is not re-filed as an inconsistency: the caller-facing
argument names (`breakpoint_id` on clear/toggle, `bp_id` on `get_traces`) and the ids themselves, which are
still `bp_1` / `exc_2` / `watch_modify_3` / `mexit_4` — see **Stop-point id**; and the internal type names
(`BreakpointInfo`, `SetBreakpointArgs`, …), which can follow the glossary whenever someone is in there
anyway. Renaming an argument breaks callers for no gain the tool name has not already delivered.
The *concepts* below keep their own names: a line breakpoint is still a breakpoint.

**Line breakpoint**:
A stop point at one source location. The only kind that can carry a condition, and the only kind that can
be deferred.

**Exception breakpoint**:
A stop point on a thrown exception of a given class and its subclasses, reporting the throw site and the
catch site.

**Watchpoint**:
A stop point on reads or writes of one field, reporting the mutating location and the field's old → new
pair.

**Method-exit request**:
A stop point on returns from a matching method, reporting which `return` was taken and the value it
produced.

**Deferred breakpoint**:
A line breakpoint whose class is not loaded yet. It holds a class-load watch instead of a real request, and
arms itself when the class appears.
_Avoid_: pending (used for the internal bookkeeping, not the concept)

### Hits, and where they go

**Hit**:
One occurrence of a stop point being reached. What happens next depends on whether it suspends — a hit
becomes either an event or a snapshot, never both.

**Event**:
A hit that suspended the debuggee and is reported to the caller, who is expected to resume it.

**Snapshot**:
A hit that was recorded without suspending: its location, thread, in-scope locals, caller chain, and
kind-specific detail, captured while only the hit thread was briefly held.
_Avoid_: log line, log entry

**Trace**:
The non-suspending mode of any stop point — snapshot the hit, resume the thread, never surface an event.
The safe mode on a shared instance, and the word this project uses throughout for it.
_Avoid_: logpoint, tracepoint

**Caller chain**:
The callers above a hit, recorded on a snapshot as locations only. Answers which path reached the hit
without the suspension that reading a full stack would need.
_Avoid_: stack, backtrace (both imply the whole stack, with locals)

**Trace budget**:
How many hits a traced stop point will record before disarming itself. Bounds work done in the debuggee,
not memory.

**Capture**:
The work one traced hit costs: reading the hit frame's snapshot and the caller chain, between the JVM
reporting the hit and the thread being resumed. The unit the reported trace cost is measured in — deliberately
narrower than "handling a hit", which also covers the condition check, the resume and our own bookkeeping.
_Avoid_: hit (a hit is the event; a capture is the work), snapshot (that is the capture's *output*)

### Arming and disarming

These three are distinct on purpose, and conflating them loses a caller's typed-in condition.

**Arm**:
To create the stop point's request in the debuggee, so it can fire.

**Disable**:
To clear the request but keep the definition — condition, trace expression, filters — so it can be re-armed
later under the same identity.

**Clear**:
To remove the stop point and its definition entirely. Unlike disable, nothing survives to re-arm.

**Disarm**:
To disable a stop point *automatically* — by the watchdog, or on a trace budget running out. Named
separately from disable because it is never the caller's instruction.

### Suspension

**Suspended**:
Held by the debugger, which is the only state in which a thread's frames and locks can be read. Counted, so
a thread suspended twice needs two resumes.
_Avoid_: stopped, frozen, paused (all read as application state)

**Blocked**:
Stopped by the application's own logic — waiting on a monitor, sleeping, parked. Independent of suspension,
and a thread can be both: a wedged thread is blocked but not suspended, so its stack stays unreadable until
the debugger suspends it as well.

**Suspend depth**:
How many outstanding suspends a thread carries. The reason a resume must ask the debuggee whether it is
actually running rather than assume one resume was enough.
_Avoid_: suspend count (JDWP's own word for the reading; depth is the accumulated state)

**Held duration**:
How long this debugger kept the debuggee suspended for an operation of its own. The cost a diagnostic
imposed on everyone else using a shared instance, as opposed to how long the operation took to answer.

**Watchdog**:
The timer that resumes a debuggee left suspended too long and disarms whatever froze it, so a forgotten
stop point cannot hold a shared instance indefinitely.

### Identity

**Stop-point id**:
The caller-facing handle for a stop point (`bp_1`, `exc_2`, `watch_modify_3`, `mexit_4`). Stable for the
stop point's whole life, including across a disable and re-arm.

**Request id**:
The debuggee's own identifier for an armed request. An internal detail that changes when a stop point is
re-armed, and deliberately not the stop point's identity.

### Safety posture

**Read-only**:
A mode in which nothing executes code in the debuggee — no method invocation, no writes, no forced returns.
A guard against accident, **not** a security boundary: anyone who can reach the debug port can do anything.

### Testing

**Probe**:
A small, checked-in Java program that reproduces exactly one failure shape, compiled and launched per test.
Each is named for the shape it reproduces, not the feature that uses it.

**Tick**:
A line a probe prints while running. The only evidence that a stop point left nothing suspended, because
the debugger reports success either way.
