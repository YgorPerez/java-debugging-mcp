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

**Attached** / **launched**:
Whose JVM it is — the fact every safety default here is derived from. An **attached** debuggee was started by
somebody else and is *presumed* shared, so suspending it may be stalling a request nobody told you about,
which is why suspensions are bounded, announced and rescued. A **launched** debuggee (LAUNCH-1) is one
`debug.launch` started for this session: nobody else is on it, so suspending it costs nobody *else* anything
and `suspend=y` — breaking before the program's first instruction — becomes reachable at all.
_Avoid_: spawned, forked (the JVM is a child process, but the word that matters to a caller is who owns it)

The asymmetry to keep straight is **who owns the lifetime**. An attached JVM's outlives the session by
definition; a launched one is bound *to the session*, so disconnecting it, dropping it, or exiting cleanly
all end the JVM, and `detach_on_disconnect` is the one way to ask for the attached shape instead. The single
gap is a `SIGKILL`ed server, which orphans it — that is why the launch reply names the pid rather than
implying a guarantee it cannot make.

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

**Augmented class**:
A **third** kind of class-you-did-not-write, and neither of the two words above covers it (DOC-6, #89). A
build-time framework — Quarkus is the case here — generates `Foo_Bean`, `Foo_ClientProxy` and `Foo_Subclass`
alongside your `Foo`, and `Foo_Subclass` **extends your own class**. So it has an ordinary dotted name, no
`SourceFile` and no useful line table, and it stands in for a class the caller wrote rather than for a lambda or
a JVM-generated proxy.
Why it needs its own word: **a frame in `Foo_Subclass` is your `Foo`**, and a reader who has learned that
source-less frames are the JVM's inventions will skip past their own bean. Any method reached through a CDI
interceptor is certain to arrive this way. `debug.list_classes` with a wildcard already shows these names, so
nothing is missing but the sentence — set the stop point on `Foo` (your code, with a line table) and expect the
stack to name `Foo_Subclass` above or below it.
_Avoid_: hidden class (the `/` suffix and the absent name are the JVM's doing; this has a real name a caller
can type), proxy (true of `_ClientProxy` and wrong for `_Subclass`, which is an inheritance relationship and is
the one you will actually be standing in)

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

### The wire

**Packet**:
One JDWP message, and **the unit this server reports its own cost in** — a dump is "~8 packets per thread",
`list_threads` is "one packet per thread name". Packets rather than bytes or milliseconds because the cost
that matters is round trips against a JVM someone else is using, and that is what a caller can reason about
before making a call.

**Framing**:
JDWP messages are length-prefixed with **no delimiter between them**, so the reader's position is only
correct if every preceding message was consumed exactly. There is no marker to seek forward to, which is
what makes losing alignment unrecoverable rather than a hiccup: the next read interprets whatever it lands
on — usually the middle of a real reply — as a length. So a lost byte does not corrupt one answer, it ends
the session (ADR-0018).
_Avoid_: resync, recover (there is nothing to resync *to*; the instinct this term exists to correct)

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
A stop point at one source location. The only kind that can carry a condition, the only kind that can be
deferred, and the only kind that forms a **wildcard family**.

**Exception breakpoint**:
A stop point on a thrown exception of a given class and its subclasses, reporting the throw site and the
catch site.

**Watchpoint**:
A stop point on reads or writes of one field, reporting the mutating location and the field's old → new
pair.

**Method-exit request**:
A stop point on returns from a matching method, reporting which `return` was taken and the value it
produced.

**Class-load watch**:
A request the debugger holds *instead of* a stop point, so a class it cannot arm yet can be armed the moment
the JVM loads one. What makes a **deferred breakpoint** and a **wildcard family** possible, and the only
reason either can reach past what is already **loaded**.
_Avoid_: class-prepare watch (`CLASS_PREPARE` is the JDWP event kind and the right name in code — a caller
is thinking about a class loading, not about an event kind)

**Deferred breakpoint**:
A line breakpoint whose class is not loaded yet. It holds a class-load watch instead of a real request, and
arms itself when the class appears.
_Avoid_: pending (used for the internal bookkeeping, not the concept)

**Wildcard family**:
The line breakpoints one wildcard `class_pattern` arms — one per matching class — together with the
class-load watch that keeps arming matches as they load (FILT-3). Every member is an ordinary line breakpoint
under its own `bp_` id; the family is a coarser handle over all of them *and* the watch.
_Avoid_: group; batch (a batch is several patterns in one call — a different thing, see **Batch**)

**A family is not deferred**, and that is a distinction the tooling makes rather than a shade of meaning:
`list_stop_points` counts families separately from its "N deferred" figure, which only ever counts
breakpoints waiting for one named class. A deferred breakpoint drops its watch the moment its class appears;
a family's watch outlives every class it arms, because the next match is more work rather than the end of a
wait. So a family is never "done" the way a deferred breakpoint is — the only things that end it are
`clear_stop_point`, `toggle_stop_point` and `panic`.

**The id prefix is `bpset_`, which says *set* where this glossary says *family*.** That is the same kind of
mismatch the **Stop point** entry records for `bp_`/`breakpoint_id`, and it is accepted for the same reason:
the id is caller-facing and already shipped (v0.7.0, pinned downstream), so renaming it costs callers
something and buys nothing the concept name has not already delivered. The window that entry describes —
"taken while nothing scripted against them yet" — closed on this prefix the day it shipped.

**Parked watch**:
A class-load watch a **wildcard family** deliberately does not hold while it is full. A full family cannot arm
the next matching class, so watching for one could only cost — an event, a suspension of the thread doing the
loading, a resume — and `max_classes` would bound what a wildcard *arms* while leaving what it *costs*
unbounded (FILT-5).

Parking is reversible by definition, and that is what makes it a distinct word: clearing a member frees a slot
and the watch comes back by itself. A **disabled** family's watch also comes back, but only when the caller
re-arms it; a watch the JVM *refused* never comes back at all. All three are "not watching", and a caller
asking "will this catch the class my next deployment generates?" needs a different answer for each — which is
why the listing gives four wordings rather than two.
_Avoid_: dormant, suspended, paused (paused especially: `debug.pause` is a whole-VM suspension and has nothing
to do with this)

**Batch**:
Several class patterns given to one arming call, each resolved independently. Distinct from a **wildcard
family**: a batch is many patterns and produces no shared handle, a family is one pattern and does. Its
defining property is that **partial success is the normal outcome**, so a batch reply is per-pattern rather
than one verdict.
_Avoid_: bulk, multi (and note a batch is not a thing that exists after the call — only its stop points are)

**Expansion**:
Turning one wildcard into the concrete classes it arms, one stop point each. Bounded by `max_classes`,
because the number is invisible to the caller before the call: `com.*` on an app server is thousands of
line-table lookups and thousands of live event requests. A reply that expanded says what it left out.

The cap counts a family's **live members**, not the classes it has ever matched — so clearing one member
frees a slot and the next matching class to load takes it. That is deliberate: the ceiling is about how many
event requests are armed on someone else's JVM at once, which is a fact about now, not a quota spent for
good. It bounds the family's **class-load watch** as well as its members — see **Parked watch**.

**Only the kinds that need a concrete target expand**, and the exception is the useful half of the term. A
line breakpoint needs a resolved location per class; an exception stop needs a reference type; a watchpoint
needs a field id — so all three expand, and all three therefore see only what is **loaded** at the moment of
the call. A **method-exit request** does not expand at all: JDWP's own `ClassMatch` does the matching, so one
request covers every class the pattern matches *including ones that load later*. That is why it alone has no
`max_classes`, and why it accepted patterns long before the other three did.

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
How many hits a traced stop point will **charge** before disarming itself. Bounds work done in the debuggee,
not memory.

Charged, not recorded — the two diverged and the difference is the point. A hit skipped by a condition or a
method filter is neither recorded nor charged, and a folded rethrow *is* recorded but not charged, so a
budget of 30 can leave more than 30 snapshots behind. What it counts is **failures**, not sightings.

**Rethrow chain**:
The run of hits produced by **one exception instance** being thrown and then rethrown as it unwinds — an
EJB interceptor chain or a Spring proxy produces one per failure. Identified by instance, since a type, a
message and even a line repeat across unrelated failures. It is one *failure*, so it costs the trace budget
once (EXC-3).
_Avoid_: duplicate throws (they are not duplicates — each is a real throw at a real site)

**Fold**:
What a rethrow chain is recorded as: the first capture, the latest sighting, and a count standing in for the
sightings between them. Both ends are kept because they answer different questions — where the failure
started (the application frame and the cause) and where it left (which wrapper let it out).

**A fold is not deduplication**, and the distinction is load-bearing rather than pedantic. Nothing here is a
duplicate: every sighting is a real throw at a real site, and #68 rejected dedupe-by-instance precisely
because a rethrow at a *different* site can be the interesting one. A fold discards a **middle**, keeping
both endpoints; dedupe keeps one representative of things treated as identical. The latest sighting is
*rolling* — nothing can know which rethrow is the last, so each supersedes the one before and whichever
turns out to be final is the one left standing.
_Avoid_: dedupe, deduplicate (see above); collapse on its own (this codebase already collapses hidden
frames, disarm notes and thread-name families, and those are three other mechanisms)

**Link**:
One step of a chained expression — `.getConfigUhList()`, `[0]`, `.getSqQuarto()`. The unit
`debug.evaluate_chain` reports in, and the thing named when a chain goes null.
_Avoid_: segment (the parser's word for the same thing; a caller never sees it)

**In-flight hit**:
A hit the debuggee has already generated but this server has not finished handling — so it can outlive the
stop point that caused it, and arrives after that stop point is disarmed and gone. Its stop point cannot be
looked up, which is not the same as the hit being spurious: it was real, it may have suspended a thread, and
something still has to resume it.
_Avoid_: stale hit, orphaned hit (both suggest it can be discarded; a traced one must still be resumed)

**Capture**:
The work one traced hit costs: reading the hit frame's snapshot and the caller chain, between the JVM
reporting the hit and the thread being resumed. The unit the reported trace cost is measured in — deliberately
narrower than "handling a hit", which also covers the condition check, the resume and our own bookkeeping.
_Avoid_: hit (a hit is the event; a capture is the work), snapshot (that is the capture's *output*)

### Arming and disarming

These four are distinct on purpose, and conflating the first three loses a caller's typed-in condition.

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

**Disarming stops future hits, not hits that already exist.** A stop point can be armed and gone while hits
it caused are still unhandled — see **in-flight hit**. Treating "disarmed" as "silent" is what froze a
debuggee in #72.

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

**Held thread**:
One thread this session froze with `debug.suspend_thread` while the rest of the JVM goes on serving. Its
own term because the whole-VM words do not fit: the debuggee is not **suspended** — a caller reading
`SUSPENDED` about it would go looking for a freeze that is not there — and the remedy is
`debug.resume_thread` rather than `debug.continue`. What it makes readable is the thread's own frames,
locals and monitors; what it does **not** make possible is **invocation**, which JDWP grants only to a
thread suspended by an event.
_Avoid_: pinned, parked, frozen (the first two are application states, the third reads as the whole VM)

**Event-suspended**:
Held because a stop point fired *on this thread* or a step landed on it, as opposed to held by a
`debug.pause` or a `debug.suspend_thread`. The distinction is invisible in every listing and decides
exactly one thing, which is why it needs a name: **only an event-suspended thread can have a method
invoked on it**. Measured, not read off the spec — a thread suspended any other way answers
`INVALID_THREAD` to an invoke while answering a full stack of locals to a frame read.
_Avoid_: "properly suspended", "really suspended" (both imply the other kinds are defective; they are
not — they read everything except an invocation)

**Suspend depth**:
How many outstanding suspends a thread carries. The reason a resume must ask the debuggee whether it is
actually running rather than assume one resume was enough.
_Avoid_: suspend count (JDWP's own word for the reading; depth is the accumulated state)

**Held duration**:
How long this debugger stopped the debuggee's application threads for an operation of its own. The cost a
diagnostic imposed on everyone else using a shared instance, as opposed to how long the operation took to
answer.

**Held is not the same as suspended**, and DISC-10 (#84) is why that had to be said. The word used to
read "kept the debuggee **suspended**", which covered `thread_dump` and the watchdog and nothing else,
because a suspension was the only way this server could stop anyone. `debug.list_instances` issues no
suspend at all — JDWP requires none for a heap query — and the JVM still stops the world for a full
live-heap walk: 522 ms of held application threads on a 2M-object heap to answer with 7 objects. Nothing
is **suspended** during that, in the counted sense this glossary defines; everything is **held**. A term
that only covered suspensions would have made the most expensive diagnostic here the one with no cost to
report.
_Avoid_: pause on its own (`debug.pause` is a specific tool), latency (that is how long the answer took,
which is the thing this is deliberately not)

**Escalation**:
Turning a hit that suspended only its own thread into a stopped VM, by issuing the VM-wide suspend from
this side. What a **conditional** stop point does on the hits where the condition holds (ADR-0020): the JVM
is asked to hold only what is needed to *decide*, and the freeze everyone else pays for is deferred until
there is something to freeze for.

Its cost is the **escalation window** — the round trip between the condition holding and the suspend
landing, during which every thread but the hit thread is still running. So a caller reading state after an
escalated hit is reading the moment after it, not the moment of it. Named rather than glossed because it is
the one promise a conditional stop point cannot make, and a tool here says what it cannot promise.
_Avoid_: upgrade, promote (both suggest the stop point changed kind; the arming is unchanged and only the
suspension widened)

**Watchdog**:
The timer that resumes a debuggee left suspended too long and disarms whatever froze it, so a forgotten
stop point cannot hold a shared instance indefinitely.

### Identity

**Stop-point id**:
The caller-facing handle for a stop point (`bp_1`, `exc_2`, `watch_modify_3`, `mexit_4`). Stable for the
stop point's whole life, including across a disable and re-arm.

`bpset_1` is a fifth kind, added by FILT-3 for a **wildcard family**. It is a distinct KIND of id in the same
namespace `clear_stop_point` already dispatches on by prefix — *not* a second way to address a breakpoint.
BP-3's one-id-per-stop-point rule is intact: each member still has its own `bp_` id and behaves exactly like a
breakpoint armed by name.

**Request id**:
The debuggee's own identifier for an armed request. An internal detail that changes when a stop point is
re-armed, and deliberately not the stop point's identity.

**One stop point can own several**, which is newer than the rest of this entry and is the thing most
likely to be assumed away. A line breakpoint holds one request *per armed location*, and there are two
independent ways to have more than one: a source line inside a `finally` resolves to several bytecode
copies, because `javac` inlines the block once per exit path (BP-4, #78); and a class name resolves to
several **reference types**, one per classloader that loaded it (BP-5, #79). The stop point is still one thing to the caller — one `bp_` id, listed
once, cleared once, and its trace budget charged once per **hit** rather than once per armed location —
so ADR-0005's one-id-per-stop-point rule is untouched. What changes is that a lookup *by* request id has
to ask whether the stop point owns the id, not whether it equals the stop point's id.
_Avoid_: "the breakpoint's request id" (there may be two, and the one that matters is usually the second
— it is the copy that fires when the code being debugged failed)

**Allocated by the debuggee, so a value may recur.** JDWP promises nothing about reuse — `HotSpot` happens
to hand them out monotonically, and this server talks to whatever is on the port. So a request id is only
meaningful *while* its request is live: remembering one and matching on it later can silently name a
different request, which is why anything keeping a set of past ids must also check that the id is not
currently in use (#72).

### Safety posture

**Read-only**:
A mode in which nothing changes the debuggee — no method invocation, no writes, no forced returns, no hot
reload, and no popped frame. A guard against accident, **not** a security boundary: anyone who can reach the
debug port can do anything.
A `dry_run` reload is the deliberate exception, because it installs nothing.
**A read can still be destructive, and read-only does not stop it** (DOC-6, #89). A single-pass stream is the
case: evaluating `response.readEntity(String.class)` on a JAX-RS `Response` **consumes the entity**, so the
application's own read afterwards gets an empty body — the live request under inspection is corrupted by
looking at it. That passes every check this mode makes, correctly: it invokes a method the caller asked for,
writes no field and forces no return. The mode's promise is about *what the debugger does*, not about whether
the debuggee's own API tolerates being asked twice, and nothing here can know which methods those are. Read at
or after the assignment to a local (where the entity is a re-readable `String`), or capture the returned value
with `debug.set_method_exit_stop` on the reading method. Before the read, only `getStatus()` and `getHeaders()`
are safe.
_Avoid_: "nothing changes the debuggee" unqualified, and "nothing executes code in the debuggee" (the wording
before SWAP-1, #58 — a hot reload invokes nothing, writes no field and forces no return, yet replaces the
running program, so it satisfied that phrasing while being the one change nothing can undo). The two
exceptions are different in kind and both matter: hot reload is a change this tool makes and reports, while a
destructive read is a change the *debuggee's own API* makes because it was asked a legal question, which no
guard here can predict

**Outstanding redefinition**:
A class a session hot-reloaded and cannot restore. Its own term because every other mutation here ends when
the debuggee resumes, while this one outlives the resume, the disconnect and the session, and changes
behaviour for everyone else on a shared instance until the artifact is redeployed. Reported when a session
ends, by `debug.list_sessions`, and by `debug.panic` — which needs it most, since clearing every stop point
and resuming every thread otherwise reads as having put the JVM back. On the same principle that makes a
counted suspension verify its resume: the debugger states what it has left behind. **Load-bearing** (SWAP-2, #61) — it is why redefinition needs
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
