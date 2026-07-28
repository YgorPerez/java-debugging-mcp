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

**Loaded**:
Present in the debuggee, as opposed to present in a source tree. A class loads on first use, so an
untouched code path contributes none of its classes, and the debuggee is the only authority on the
question. Load-bearing because JDWP can only report what is loaded: "not loaded" and "no such class" are
indistinguishable from outside, so a tool must offer both readings rather than pick one.

There is a **third** case, and unlike the other two it is not a limit of JDWP — it is ours. SIG-1
([#46](https://github.com/YgorPerez/java-debugging-mcp/issues/46)): a class can be loaded, sitting in the
very list the tool just searched, and still be missed because the tool spelled its name differently from
the JVM. Every lambda was rendered `Outer$$Lambda.0x…` where the JVM, a stack trace and `jstack` all say
`Outer$$Lambda/0x…`, so a caller who pasted the real name got `0/0` and was told the class might not have
loaded yet. The two-reading rule above quietly assumes the tool's own spelling is not the variable. Where
it might be, the tool must **check before it blames** — `debug.list_classes` re-reads the loaded names
with `/` and `.` treated alike and, when the class is there, names the spelling instead. "Not loaded"
about a class the debugger is looking at is not one of two honest readings; it is a wrong answer.
_Avoid_: exists, defined (both invite the source tree as the authority)

**Hidden class**:
A class the JVM made rather than a compiler — what is actually behind a lambda, a method reference or a
generated proxy. Named `<class>/<a suffix the JVM assigned>`, where the `/` is part of the name rather
than a package separator, and carrying no line table, so its frame is real but has nothing to look up.
The name a caller is shown is always the one `Class.getName()` and a `jstack` dump use, even though the
debuggee spells the boundary differently on the wire depending on its version. **A name this tool shows
is a name it accepts** (DISC-4, #50): asking about a hidden class under the name a stack printed works on
every supported JDK, because the resolver offers the debuggee both wire spellings instead of deciding for
itself which JVM generation it is talking to.
_Avoid_: synthetic class (the compiler's own inventions — a `lambda$…` body, an `Outer$1` — are ordinary
classes and methods with real names and real source lines; one is actionable and the other is not, and
a word covering both loses exactly that)

**Source drift**:
The checkout in front of you not being the build that is running. A finding, not an error — the debuggee
reports which file a class was compiled from, and a mismatch is the answer to a question rather than a
failure to answer it.

**Stale bytecode**:
The narrower and commoner case of the above, and the one `SourceFile` cannot see: the same class, from the
same file, compiled *earlier* than the build on disk. `debug.source` answers "which file"; `debug.check_stale`
answers "which build of it", by comparing line tables (DISC-7). Worth keeping distinct from source drift
because the remedies differ — one means you are reading the wrong file, the other that the JVM is running
last week's compile, and only the second is fixable with `debug.reload_class`.
_Avoid_: "out of date", which does not say which side is behind.

**Class root**:
Where the package tree starts in the **build output** (`target/classes`), as against a **source root**
(`src/main/java`), which is where it starts in the sources. A compiled class is looked for at
`<class root>/<package as directories>/<SimpleName>.class` — note that this uses the class's own name,
including `$` for inner classes, where a source lookup uses the file name the JVM reports. Two lists, not
one, per ADR-0016.

**Hot reload**:
Replacing a loaded class's bytecode in a running JVM (`RedefineClasses`) — no redeploy, no restart, warm
state intact. `HotSpot` accepts **method bodies only**. Not the same as a *redeploy*, which discards the
classloader and everything it held, and not the same as a **classloader reload** (what `ReloadProbe` does),
where a new type with new JDWP ids replaces the old one; a hot reload keeps the `referenceTypeID` and
replaces the code behind it, which is exactly why ADR-0011 refuses to cache line tables per connection.

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
A hit that suspended the debuggee and is reported to the caller, who is expected to resume it. Reported
**two** ways, and both always happen: recorded in a bounded buffer the caller polls, and pushed as an
alert. The buffer is the record; the alert is a hint that one exists.

**Alert**:
Something the debugger says without being asked, because the debuggee's state changed under the caller —
a stop point suspending the VM, or the watchdog resuming it and disarming whatever froze it. Best-effort
by definition: an alert may be dropped, and everything one carries is also readable by asking, so nothing
depends on one arriving.
_Avoid_: notification (JSON-RPC's word for any id-less message, including the inbound ones this server
receives — the wire method stays `notifications/message` because that name belongs to the protocol, not
to this concept)

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

**Finished**:
Run to completion. JDWP still answers `ZOMBIE` for it while the debugger holds the `Thread` object, so it
is a thread the debugger can name and describe but never read — and never suspend. The opposite answer to
**running**, and DUMP-4 (#47) is what happens when a reply confuses the two.
_Avoid_: dead, zombie (the first reads as a fault; the second is JDWP's wire word, worth quoting in a
message but not the concept's name)

**Vanished**:
Listed by the JVM and already gone by the time the debugger asked about it — the id is invalid, so there
is nothing to name or describe. A thread id is a weak reference, so on a pool that retires workers this is
the ordinary case rather than the exotic one, and it is a third reason a dump is short, alongside the
`limit` and the suspension budget. Distinct from **finished**: a finished thread is still readable as a
row, a vanished one is only a count.
_Avoid_: dropped, lost (both suggest the debugger mislaid it)

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
A mode in which nothing changes the debuggee — no method invocation, no writes, no forced returns, no hot
reload, and no popped frame. A guard against accident, **not** a security boundary: anyone who can reach the
debug port can do anything.
_Avoid_: "nothing executes code in the debuggee", the wording before SWAP-1 (#58). A hot reload invokes
nothing, writes no field and forces no return, yet replaces the running program — so that phrasing
described the mode as permitting the one change nothing can undo. A `dry_run` reload is the deliberate
exception, because it installs nothing.

**Outstanding redefinition**:
A class a session hot-reloaded and cannot restore. Its own term because every other mutation here ends when
the debuggee resumes, while this one outlives the resume, the disconnect and the session, and changes
behaviour for everyone else on a shared instance until the artifact is redeployed. Reported when a session
ends and by `debug.list_sessions`, on the same principle that makes a counted suspension verify its resume:
the debugger states what it has left behind. **Load-bearing** (SWAP-2, #61) — it is why redefinition needs
no permission axis of its own, since reporting an unrepairable side effect is more honest than a mode
nobody remembers to set.
_Avoid_: leaked, dirty (both suggest a fault; the swap was asked for, and the residue is a fact about the
JVM rather than a defect in it)

### Testing

**Probe**:
A small, checked-in Java program that reproduces exactly one failure shape, compiled and launched per test.
Each is named for the shape it reproduces, not the feature that uses it.

**Tick**:
A line a probe prints while running. The only evidence that a stop point left nothing suspended, because
the debugger reports success either way.

**Running** (of a probe):
Having executed code — as opposed to **listening**, which is all a successful attach proves. The JDWP agent
binds during JVM startup, before the main class is loaded, so the two are minutes apart on a slow enough
runner and indistinguishable from the debugger's side. A test whose first question is about loaded state
(`debug.list_classes`, `debug.list_methods`, `debug.list_fields`, `debug.source`) has to wait for the
probe's readiness line first: those tools answer "not loaded" *correctly*, so losing the race does not
fail loudly, it asserts a wrong finding and blames the tool (TEST-17, #49). `Probe::launch_running` is
that wait.
_Avoid_: "started", "up", "attached" — every one of them reads as either state.

**Cassette**:
A recorded JDWP session — every request and the reply it got — kept in a file and served back to the
debugger with no JVM behind the port. A snapshot of one debuggee on one JVM, so it complements a probe
rather than replacing one: it cannot notice the debuggee changing, and it can be *edited* into a shape no
JVM here could be asked to produce. See ADR-0014.
_Avoid_: mock, stub, fixture (the first two suggest something written to satisfy the test; a cassette is a
transcript of a real session, and its authority comes from that)

**Miss**:
A request a cassette has no recorded answer for. Never answered — the connection is dropped and the command
is named — because a plausible-looking error reply would let a replay test pass while proving nothing.
