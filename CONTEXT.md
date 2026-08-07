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

**Head** (of an expression):
The **first** segment of a chain, and the only segment whose meaning is not "a member of the thing before
me". A head is resolved against the world — a local, `this`, a bare field of the frame's own class, a class
name for a static, or an `@0x…` object handle — where every segment after it is resolved against the value
its predecessor produced.
The word is needed because the difference is a *rule*, not a formatting detail: a handle may only ever be a
head (`something.@0x1f4c` is meaningless and is refused as such), and a one-segment name resolves through
Java's own shadowing order where a later segment never does.
_Avoid_: root, receiver (the receiver is what a segment is resolved *against*, which is the previous
segment's value — the opposite end), base

**Bound head** (of a condition):
A name a `condition` may use that is not in the frame, because the **hit** carries it rather than the frame:
`exception` on an exception stop, `newValue` on a field stop (ADR-0034). Reserved, exactly as `this` is, and
bound by rewriting to the `@0x…` handle when the value is an object. A method-exit hit binds nothing — its
frame is the returning method's own.
_Avoid_: variable, magic name, implicit local (it is none of those — it is not in the variable table at all)

**Generic type**:
The type as the *source* declared it, read from the class file's optional `Signature` attribute:
`List<ReservaHotel>`. The counterpart of **erased type** below, and defined against it rather than beside it —
every type string the tool emits is the generic one where the attribute exists and the erased one where it
does not (ADR-0033).
The distinction earns a word because the generic type is what makes the *next* expression writable without a
guess: on a DTO graph, seeing `List` and having to guess the element type is an error and a retry, where
`List<ReservaHotel>` composes straight into `lines[0].getSku()`.
_Avoid_: raw type (a real and different Java concept — see **erased type**, which exists precisely in order
not to assert it), reified (Java has none), declared type (true but says nothing about the parameters, which
are the whole point)

**Unfetched** (of a lazy association):
A Hibernate entity proxy or persistent collection whose row or contents nobody has loaded. A **third
answer** alongside a value and `null` (ADR-0032): the row very likely exists, and resolving through it is
what would fetch it — issuing the deferred SELECTs into whatever persistence context the debuggee thread is
in, or throwing `LazyInitializationException` if the entity is detached.
The word is `unfetched` and not the more obvious `unloaded` for two reasons. **`unloaded` is already taken**,
by a class the JVM has not loaded — see **loaded** below — so reusing it would collide with a correct existing
use of the same word in the same glossary. And `fetch` is the domain's own word: `FetchType.LAZY` is the
annotation on all 1897 of them.
_Avoid_: unloaded (see above — it means a class here), uninitialised (Hibernate's own word, but it reads as
"not constructed"), empty (what an unfetched collection is mistaken FOR), null (what it is not — see
**unbuilt source** for the same distinction about a class)

**Invoke-free** (of a read):
A read that runs **no code in the debuggee** — it asks the JVM for state and never calls a method on the
object it is reading.
**It earns a name because three hazards found separately here are the same hazard, and being invoke-free
rules out all three at once**: it cannot **fetch** an **unfetched** association (ADR-0032), it cannot consume
a single-pass stream the way evaluating `readEntity` does (see **Read-only**), and it cannot wedge on a
monitor the hit thread does not own (ADR-0036). A trace **snapshot**'s locals, an anonymous class's captured
locals, a chain walk's links and a query row's fields are all read this way, by the same means — no thread is
supplied to render with, so there is nothing to run a method on. The name arrived late: each of those sites had
already reasoned its way to the same rule separately, and the chain walk's own note gets to the monitor
consequence unprompted.
**`shallow` is not this word, and reading it as this word is the trap.** A shallow render calls `toString()`
whenever it has a thread; a deep one walks fields. So the shallow/deep axis is close to the *inverse* of this
one, which is why "read-only falls back to shallow" and "use `expand_objects`, which invokes nothing" are
both true and read like a contradiction. Depth is not the question — whether a thread was supplied is.
**Bounding the depth cannot substitute for it**, because on a JPA entity the first level is already the
hazard: its own `toString()` routinely names its associations.
_Avoid_: shallow (means depth here, and points the wrong way — see above), projection (JPA's own word for
selecting a subset of columns, `select r.codigo, r.status`; using it for this would suggest a tool rewrites
the query it was asked to run), side-effect free (true, and says nothing about what makes it so), fetch-free
(names one of the three consequences as if it were the property), read-only (a session mode, and a read-only
session still invokes on this path — see **Read-only**)

**Uncancellable invocation**:
JDWP offers no way to abort a method invocation once it has been issued, so a call that outruns the invoke
budget goes on running in the debuggee after this side has stopped waiting for it.
**The budget frees the DEBUGGER and never the debuggee**, and every surprise here follows from that one
asymmetry — each reasoned out separately at its own site before the property had a name. An invocation
needing a monitor the hit thread does not own cannot complete at all, which is why an invoking
**trace_expr** is refused on `blocked` (ADR-0036). One that merely outruns its budget leaves the JVM to
re-suspend that thread when the call finally returns, 1.2 s later and for good (see **Trace**). A
`toString()` that outruns it leaves the thread executing it, so that thread's frames stay unreadable until
it finishes. And a LATER invocation on that thread earns JDWP's `ALREADY_INVOKING`, since a thread may have
only one in flight.
**The last is what earns the word.** TRACE-13 (#131) read an `ALREADY_INVOKING` as two of the caller's own
expressions colliding, and asked for a **capture**'s invocations to be serialised — they already are, one at
a time in one event pump. The collision is with a call nobody is waiting for any more, so no ordering on
this side can prevent it, and what the reply owes is that explanation rather than the wire code.
_Avoid_: cancelled (the one thing that cannot happen to it), timed-out invocation (names our clock as
though it ended the call), orphaned (discouraged for an **in-flight hit** already, and wrong here for the
same reason — it suggests something that may be dropped), hung (suggests the debuggee is stuck, when it is
usually only slow)

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

**Copy**:
One of the **reference types** a class *name* resolves to — one per classloader that defined it. Not a
synonym for the class: `br.com.infotera.common.util.Utils` genuinely exists several times in a WildFly JVM,
each with its own statics, so a value read from the wrong one is a wrong answer rather than a slow one
(BP-5, [#79](https://github.com/YgorPerez/java-debugging-mcp/issues/79)). Every reply that had to choose
between copies says so, and `Name@0x<loader>` pins one.

**A redeploy is what makes the copies disagree, and that is what makes this a word rather than a detail.**
The retired deployment's module classloader keeps its copy loaded, so one name resolves to the old
bytecode and the new bytecode at once — and the old copy is the one that sorts first. Two failures come out
of that single asymmetry and they look nothing alike from the caller's chair. A **member lookup** against
the stale copy fails *loudly and misdirectingly*: it blames the caller's signature for a member the running
code has, sending them to re-check an arity that was never wrong (EVAL-13,
[#116](https://github.com/YgorPerez/java-debugging-mcp/issues/116)) — so a lookup now tries every copy, and
says which one answered. An **armed stop point** on the stale copy failed *silently*: it stayed listed,
stayed enabled, and never fired, which is indistinguishable from the hypothesis about the code being wrong
(BP-7, [#115](https://github.com/YgorPerez/java-debugging-mcp/issues/115)) — so an exact name now keeps its
class-load watch for life and arms the new copy itself (ADR-0028). The two fixes are asymmetric on purpose,
and the asymmetry is the point: the loud one could be *answered better*, the silent one had to be
*prevented*, because nothing a reply says can reach someone who is reading an absence.
_Avoid_: "the class" once more than one copy is loaded (that names a name, not a type); **reload** for this
(a copy is a second *definition* under a second loader, not a redefinition of the first — see the entry
under **The target**)

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

**Erased type**:
A type shown without its parameters because the debuggee had no generic signature to give — **not** a claim that
the declaration was raw.
The two readings arrive identically, which is why this needs a word. A generic signature is an *optional*
class-file attribute — absent from code compiled without it, from some synthetic members, and from arrays of type
variables — and JDWP answers with an **empty string** rather than an error. But `List` declared raw and
`List<ReservaHotel>` whose signature was stripped are different facts about the code, and the caller's next move
differs: in the first there is nothing more to know, in the second the element type exists and has to be reached
another way, by reading an element and asking its runtime type.
It only reads as ambiguous where the class *declares* parameters. `java.lang.String` has nothing missing and
saying so would be noise; a bare `java.util.List` is the case that owes the caller a word.
_Avoid_: raw type (a real thing in Java — a declaration that named no parameters — and precisely the reading
this term exists in order *not* to assert)

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

**Unbuilt source**:
The `.java` on disk being ahead of the `.class` compiled from it — the third position in the chain, and the
one neither term above reaches. Source drift is *wrong file*; stale bytecode is *the JVM is behind the
build*; this is *the build is behind the editor*, so you are reading a statement nothing has compiled yet.
Measured rather than hypothetical: in the target environment the class roots are byte-identical to the
deployed jars and 2–3 commits behind `src/main/java`, so both checks above report clean while
`debug.source` renders a later version of the method being debugged (DISC-11, ADR-0029). Two evidences
reach it and they are of unequal strength — a source file too *short* to hold a line the compiler emitted
an entry for is a proof, an mtime newer than the `.class` is only a hint, because a checkout moves an mtime
without changing a byte.
_Avoid_: "stale source", which inverts which side is behind — the source is the *newest* thing in the chain.

**Structural change**:
An edit `RedefineClasses` can never install, whatever you do to it: a field or method added, removed or
re-modified, a changed signature, a changed class modifier, a different superclass or interface list.
`HotSpot` permits **method body changes only**, so this is the dividing line between "hot-reloadable" and
"needs a restart" — and it is close to a coin flip in practice rather than a rare case (151 of 300 recent
commits in the target repo). `debug.check_stale` forecasts it before any attempt (DISC-13, ADR-0030).
Independent of **stale bytecode**: a class can be both behind the build *and* illegal to swap, and the
remedies differ.
_Avoid_: "breaking change" (that is about a class's callers, not about what the JVM will accept) and
"incompatible" (true of the refusal, but it does not say the shape is what made it so).

**Forecast**:
A verdict reached from a class file rather than from the JVM's answer, and therefore held to a different
standard depending on which way it points. A predicted **refusal** is stated confidently, because six of
`HotSpot`'s twelve codes follow from the shapes alone. A predicted **pass** says only "no structural change
detected" and never promises a swap will land, because the other six — a verifier rejection, an
`INVALID_TYPESTATE` against instances that already exist, an unreadable class-file version — are invisible
to it. **A pre-flight that over-promises is worse than none.**
_Avoid_: "validate" and "check" for the positive direction, both of which imply the answer was settled.

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

**Reply**:
What a `debug.*` tool hands back to the caller — the rendered text a model reads, not anything on the socket.
Every claim this glossary makes about what a reply "says", "prints", "owes" or "left out" is about that text.

**A reply packet is the other thing, and it is always spelled with the noun.** One JDWP message answering one
command, correlated to it by packet id — a **packet**, in the unit sense defined below. The two are not
related by a rendering step: a single reply is assembled from many reply packets, and the tools that report
their own cost are reporting how many.

**This entry exists because the bare word was doing both jobs, undefined, forty lines apart in this very
section** — "the middle of a real reply, as a length" is a packet, "a reply that prints a duration" is caller
text. Twelve or so uses meant the caller sense against six meaning the packet, `docs/toolkit-contract.md`
already made "change what a **reply says**" a row in its caller-visible risk table, and nothing anywhere
defined either. The bare word therefore goes to the caller sense on the numbers and on the contract doc, and
the wire sense pays the two syllables. It is placed here, in the section about the socket, precisely because
this is where a reader meets both and has no way to tell them apart.
_Avoid_: response (nothing here uses it and it invites the HTTP request/response frame, which JDWP does not
have — see the `_Avoid_` on **independent reads**); output (the tool's stdout is a different thing, owned by
one task, ADR-0012); bare "reply" for the packet

**Packet**:
One JDWP message, and **the unit this server reports its own cost in** — a dump is "~8 packets per thread",
`list_threads` is "one packet per thread name". Packets rather than bytes or milliseconds because a packet
count is deterministic and independent of machine load, where a duration is neither, and because it is what a
caller can reason about before making a call.

**It is no longer the same number as the round trips**, and this entry used to say it was — it justified
packets-as-cost-unit by equating them. On any path using **independent reads** the packet count is unchanged
while the waits are divided by up to the cap on concurrent reads (ADR-0038), so a reported packet figure is
now an **upper bound** on what a caller waits for rather than a proxy for it. What it still says exactly is
what the server *sent*, which is why the cost lines kept it and gained the other number beside it.
_Avoid_: using "packets" and "round trips" as if they were interchangeable, which is what this paragraph
exists to stop — see **round trip** for which claim belongs in which unit.

**Round trip**:
One wait for the debuggee to answer. The unit a caller's **latency** is measured in, where a **packet** is the
unit its **traffic** is measured in.

**Two units exist because they stopped being one number.** Until **independent reads**, every packet was
awaited on its own, so a packet count answered both questions and the glossary needed only the one word. It
now answers only the second: a set of independent reads crosses the wire as many packets and is waited on
about once. A reply that prints a duration or a freeze is describing round trips; a reply that prints a cost
is describing packets; and a claim about one made in the units of the other is the error this pair of entries
exists to prevent.

**A reported round trip count is derived rather than observed**, from the cap on how many reads may be
outstanding at once — so it is a tight lower bound on the waits, not a measurement of them, and every reply
that prints one prints it with a `~`. A packet count has no such caveat, which is the other reason the cost
lines still lead with it: it is deterministic and comparable between releases where this is neither.

_Avoid_: hop (a network term for something else entirely — a round trip here may cross several); latency
(the property, not the unit; a round trip is what you count, latency is what one costs)

**Framing**:
JDWP messages are length-prefixed with **no delimiter between them**, so the reader's position is only
correct if every preceding message was consumed exactly. There is no marker to seek forward to, which is
what makes losing alignment unrecoverable rather than a hiccup: the next read interprets whatever it lands
on — usually the middle of a real reply packet — as a length. So a lost byte does not corrupt one answer, it
ends the session (ADR-0018).
_Avoid_: resync, recover (there is nothing to resync *to*; the instinct this term exists to correct)

**Independent reads**:
Reads whose requests do not depend on each other's reply packets, and which may therefore be in flight
together.

**A property of the reads, not of the code that issues them** — which is why it was defined here before
anything exploited it. Every read this server makes is already independent or dependent; PERF-1
([#100](https://github.com/YgorPerez/java-debugging-mcp/issues/100)) *acted* on the distinction, and the term
existed first so that work had something to be precise against.

**It names a licence, and the licence is per call site rather than per kind of read.** That is the whole
weight of it: independence has to be *established* for a particular sequence, never assumed from the shape of
the commands. Three real sequences are dependent, and each fails for its own reason — a suspend must land
before a frame is read at all; a frame's variable names must be known before its values mean anything; and a
watchpoint's **old value** is only readable while the pending store has not yet committed, so that read
cannot be moved out of its window. A term that made independence sound like a property of *reading* rather
than of a *sequence* would quietly license all three.

What it buys is **round trips**, not **packets** — the same commands are still sent, so it is not a cheaper
read but a shorter wait, and a shorter **suspension window** wherever the reads happen under one.

_Avoid_: pipelined (borrowed from HTTP, where it promises responses come back IN ORDER — JDWP correlates a
reply packet to its command by packet id and they may arrive in any order, so the word asserts the one thing
that has to be *proven* rather than assumed); batched (taken, and by something close enough to confuse: a
**batch** is many class patterns given to one arming call, and it also has partial failure as a normal
outcome — and it implies one combined request answered by one reply packet, which JDWP has no such thing as);
concurrent (true of the mechanism and silent about the property that makes it safe)

**Wave**:
A set of **independent reads** issued together and awaited together.

**Where independent reads is the licence, this is the mechanism** — and the two are separate entries because
the licence is the part that has to be established and the mechanism is the part that cannot check it.
Anything may be issued as a wave; only an independent set may *correctly* be.

**Every reply packet is awaited, including after one has failed.** A wave is not an all-or-nothing request:
it has no combined reply packet, its results answer its reads one for one, and a failure is one of those
answers rather than the end of the set. That follows from the wire rather than from taste — the commands are already sent
and JDWP has no way to recall one, so abandoning the wait would abandon only the answer while the debuggee
does the work regardless.

**A wave is bounded, so a large set of reads is several waves rather than one.** The bound is what keeps the
mechanism from becoming a way to have unlimited work outstanding, and it means the saving is bounded too: a
fan-out wider than the bound is divided by it, not eliminated.

_Avoid_: batch (taken — see the `_Avoid_` above); pipeline (same objection as above); burst (describes the
traffic's shape on the wire and says nothing about the reads being independent or the reply packets being
awaited)

**Speculative read**:
A read issued for something that may turn out never to have been needed.

**The one thing that can make a wave cost more than the loop it replaces**, and therefore the invariant every
conversion to a wave is held to. Gathering a fan-out's reads into one set invites reading for *every*
candidate, and the candidates that get discarded are pure loss — measured in **packets**, which is the unit
that is supposed to be unchanged by any of this. A wave that saves round trips by spending packets has moved
the cost rather than removed it, and has done so in the unit a caller's cost line reports.

**It has two shapes here, and both are decided by something the reads cannot see.** A filter may discard a
frame or a row before anything about it is read, so the filter is resolved first and only survivors are read
for. And a budget may stop a walk part way, so how many of a level's children will be read is not known until
they have been.

Neither shape makes a fan-out permanently speculative. Both make the *set* the thing that has to be settled
before the reads go out, which is **committed values**.

_Avoid_: prefetch (describes *when* a read happens, not whether it may be wasted — a prefetch for something
already known to be needed is not speculative, and most of the ones here are not)

**Committed values**:
The values a caller has established *will* be rendered, before any of their reads are issued.

**This is what a prefetch needs in order not to be a speculative read**, and it is the reason the entry above
can reject the word *prefetch* rather than the practice. A read is speculative because of what the *caller*
does not yet know, so the fix is on the caller's side: establish the set first, then read for it. PERF-2
([#129](https://github.com/YgorPerez/java-debugging-mcp/issues/129)) grants it on the row-projection path and
on the deep render.

**A caller establishes commitment one of two ways, and the second is what a budget forces.** It may simply
*hold* the set: a row projection has every row's field values in hand and applies its own field cap, so what it
will render is enumerable before any read goes out. Or it may *prove* the set, which is what a shared node
budget leaves it with — a deep render spends that budget per node as it goes, so a level's later children are
not knowably reached by holding the list. The proof is arithmetic against the budget: children are **certain**
when the budget cannot run out before the walk arrives at them, whatever the nodes ahead of them spend.
Certainty is a property of a *prefix*, so a level is committed as far as the arithmetic reaches and read one at
a time beyond that.

**Committing bounds the set; it does not choose the reads.** What may be waved for a committed value is its
*first* read — the one the renderer issues before it has decided anything, unconditionally. A second read that
the first read's answer decides (a boxed primitive's `value`, once its type says it is one) is a different
licence and has to be established separately, because between the two there is a reply the caller has to see.

**A commitment is only as good as its window.** Committing says the value will be rendered; it does not say
the object will still be there when it is. An `InvokeMethod` between the wave and the render runs arbitrary
debuggee code, so a value read before it and printed after it describes the object as of the *read*.

The shallow grants close that window outright by rendering with no thread to invoke on. The **deep** grant
cannot — it renders at the depth limit via `toString()` — and takes the window knowingly, because what it can
cost is bounded: a class never changes and a `String`'s contents never change, so the only reachable failure is
the object being collected in between, where a live read would have failed and a prefetched one prints a name
that *was* true. A snapshot rather than a wrong answer, and not new in kind — a level's values have always been
read in one go and then rendered one at a time, so the ids were already held across invocations.

Distinguish from **a store committing**, in the `independent reads` entry above: there it is the debuggee's
write landing in memory, and the subject is the JVM. Here a *caller* commits **to** rendering something, and
the subject is this server. Same word, different sense, and worth stating because both senses are about
timing.

_Avoid_: reserved (taken by the other resolution of the budget question — *reserving* budget breadth-first is
the caller-visible alternative to committing, so the two words name opposite answers); planned (says a
decision was made and nothing about it being binding, which is the entire content of this term); pinned
(means keeping an object alive against collection, which is `DisableCollection` and is exactly what ADR-0022
refuses to do)

### Stop points

**Stop point**:
Anything armed in the debuggee that reports when execution reaches it. The umbrella over all five kinds.
_Avoid_: breakpoint (when you mean any kind rather than a line breakpoint specifically)

The **tool names follow this**, as of VOCAB-1 (#20): `debug.set_line_stop`, `debug.set_exception_stop`,
`debug.set_field_stop`, `debug.set_method_exit_stop`, and `debug.clear_stop_point` /
`debug.list_stop_points` / `debug.toggle_stop_point` across all of them. `debug.set_monitor_stop` (DUMP-7)
was named to the same pattern from the start, which is the dividend of having taken the renames. Before that, `breakpoint` named
three different scopes depending on where you read it — one source location in `set_breakpoint`, two
things that were not source locations in `set_exception_breakpoint` and `set_method_breakpoint`, all four
kinds in `clear_breakpoint` / `list_breakpoints` / `toggle_breakpoint`, and `set_watchpoint` was a stop
point that the word did not cover at all. The renames were taken while nothing scripted against them yet;
the window for doing it cheaply does not reopen.

Two things deliberately did **not** change, so this is not re-filed as an inconsistency: the caller-facing
argument names (`breakpoint_id` on clear/toggle, `bp_id` on `get_traces`) and the ids themselves, which are
still `bp_1` / `exc_2` / `watch_modify_3` / `mexit_4` / `mon_blocked_5` — see **Stop-point id**; and the internal type names
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

**Monitor stop point**:
A stop point on lock contention: one of the four `MONITOR_*` events, reporting which lock, which thread, and
the code that blocked or waited (DUMP-7). The fifth kind, and the only one whose site the caller does **not**
choose — contention happens wherever threads collide, which is why a suspending one needs a `thread_id` and
why there is nothing else to narrow it to.
_Avoid_: contention breakpoint; lock breakpoint (a "breakpoint" implies a location you picked)

**Monitor pair**:
The **two** events that delimit one waiting period, and the unit a duration belongs to: `blocked` → `acquired`
for a contended entry, `wait` → `waited` for an `Object.wait()`. Four event kinds, two pairs.
_Avoid_: monitor bracket (see below), monitor cycle (nothing repeats)

**A duration is a property of the pair, never of an event**, because no monitor event carries one — see
**debugger-measured** below. Arming one half is legitimate and cheaper: it answers "is anything blocking at
all" for one request instead of two, and its snapshots say the duration was not measurable rather than
printing a zero.

**The two pairs are named apart** (`blocked_for`, `waited_for`) rather than sharing one `elapsed`. Blocking
is involuntary and a long one is a fault; `wait()` is voluntary and a long one is often a healthy idle worker.

**Whether the thread OWNS the monitor differs per event, and `blocked` is the odd one out.** This is the fact a
snapshot's own subject makes easy to get wrong, so it is worth stating rather than inferring: at `blocked` the
thread is *attempting* to enter a monitor another thread holds, so it owns nothing; at `acquired` it has
entered, so it owns it; at `wait` it owns it, because Java requires holding a monitor to call `wait()` on it;
and at `waited` it has **re-acquired** it — which this entry used to leave open ("not something this project
has measured") and DUMP-8 measured: an invocation needing the monitor answered promptly there, on Temurin
21.0.12.
It matters because it decides what may safely be *asked* at a hit. An invocation needing the monitor re-enters
harmlessly where the thread already owns it, and cannot complete where it does not — and an invocation is
**uncancellable**. So `blocked` is the one event of the four whose natural question (something about the object being
contended) is also the dangerous one, which is why this kind has no **condition** at all and why an invoking
**trace_expr** is refused on it and on nothing else (DUMP-8, #123, ADR-0036).

**The rule is ownership, not pair position, and the difference is not academic.** DUMP-8's first cut refused
`wait` too, on an "opening half of a pair" framing — and this entry is what caught it, because `wait` is an
opening half whose thread *owns* the lock. The same measurement carried its own control: on one `waited` hit
an expression naming the reported monitor returned while one naming a **different** lock, held by another
thread, timed out. So "can this invocation complete" is a question about *which* monitor the expression needs
and who holds it, and only the first half of that is knowable when a stop point is armed.
_Avoid_: opening half / closing half as a **safety** distinction (they are a real pair structure and the right
words for duration, but they do not predict ownership — `wait` is the counter-example)

**`bracket` was the first candidate and lost on a collision**, which is the same test that chose **unfetched**
over `unloaded`: *bracket* is **already taken** in this codebase, by the `[…]` of a subscript expression — the
parser splits on `.` at bracket depth and reports unbalanced brackets. A second meaning 3,000 lines away in the
same file is a homonym, not a synonym, so the word is left where it already works. "Open" and "close" still read
better with it than with *pair*, and that is not enough.

**Debugger-measured**:
A figure this server computed rather than read off the wire, labelled as such wherever it is printed. A
**reported** figure is the debuggee's own account — a line number, a returned value, a `timed_out` flag — and
the two must never be printed as though they were the same kind of thing.
_Avoid_: elapsed; measured (unqualified — the point of the term is *whose* measurement it is)

**Several predate the term.** A traced stop point's mean capture, arrival rate and capture share (TRACE-7),
and the duration `debug.thread_dump` held the VM (#17). What the term was finally needed for is a
**monitor pair**'s duration (ADR-0035), because that is the first case where the wire offers a *look-alike*:
`MONITOR_WAIT` carries a `timeout` field, which is the value the caller passed to `wait(…)` and not how long it
waited. Printing that as a duration would have been plausible on every reply, which is a worse failure than
having no figure at all.

**The test for whether a figure needs the label is what it DESCRIBES, not who computed it** — and getting that
backwards is a live risk, because the older figures above do not carry the label and a reader could take them
for omissions. They are not. `Trace cost` describes *this server's* own cost and says so in its name, so there
is nothing a caller could mistake it for; it is already unambiguous, and its no-data case is honest in the same
way ("UNMEASURED rather than free"). A monitor duration is the opposite: it describes something about the
**debuggee** — how long its thread was blocked — while being computed here, which is the only shape where a
caller can reasonably read our number as theirs. So the label belongs to figures that cross that line, and
adding it to a figure that describes us would be noise.

So the label is not modesty about precision, it is a claim about **provenance**. A monitor duration is
timestamped at the opening event and subtracted at the closing one, which means it includes our own capture
latency (~0.86 ms per hit before caller frames) — noise against the multi-second block a wedged server is
asked about, a material fraction of a 5 ms one. A caller cannot tell which case they are in unless the reply
says whose number it is.

**Class-load watch**:
A request the debugger holds *instead of* a stop point, so a class it cannot arm yet can be armed the moment
the JVM loads one. What makes a **deferred breakpoint** and a **wildcard family** possible, and the only
reason either can reach past what is already **loaded**.

**It costs a filter evaluation, not an event, and the difference is the whole of what it is cheap enough for.**
The watch names a class pattern the **debuggee** itself tests as each class loads, so a class that does not
match is never reported and costs no packet, no suspension and no resume. For an exact-name stop point that
pattern is one signature, so the only thing that fires it is *that* class loading again — which is a redeploy
(ADR-0028). A **wildcard family**'s pattern is broader and does fire per matching load, and every one of those
is accounted for in the listing: armed, or no such method, or refused because the family is full.

Written down because it was read the other way round — as an event per class *loaded* — which turns a
per-redeploy cost into a per-classload one and makes the watch sound like something to ration. It also
produced a wanted-but-unbuildable counter: there is nothing left to count, since the non-matching loads raise
nothing and the matching ones are already reported.
_Avoid_: class-prepare watch (`CLASS_PREPARE` is the JDWP event kind and the right name in code — a caller
is thinking about a class loading, not about an event kind)

**Deferred breakpoint**:
A line breakpoint whose class is not loaded yet. It holds a class-load watch instead of a real request, and
arms itself when the class appears.
_Avoid_: pending (used for the internal bookkeeping, not the concept)

**Wildcard family**:
The line breakpoints one wildcard `class_pattern` arms — one per matching class — together with the
class-load watch that keeps arming matches as they load (FILT-3). Every member is an ordinary line breakpoint
under its own `bp_` id; the family is a coarser **id** over all of them *and* the watch.
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

**A stop-point set is a different thing**, despite being one word from what this prefix says. That is an
export artefact — a *description* of armed stop points, which can contain a family among its entries — where a
family is a live collection. The two nest; they do not compete. Cross-referenced in both directions because the
collision was noticed while the second term was still unreleased and was **kept deliberately** rather than
missed.

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
family**: a batch is many patterns and produces no shared id, a family is one pattern and does. Its
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

**A hit is counted whether or not it is reported.** A stop point with a false **condition** hits, is dropped, and
lets the thread go; a narrowed method-exit request hits on every method of the class and drops the ones that do
not match. Those are hits. The distinction is the whole diagnostic value of counting them: a stop point showing
many hits and no events means the condition never held, while one showing none means the code never ran, and
those send a reader somewhere completely different.

**Hit tally**:
How many times a stop point has been hit. An observed count, growing as the debuggee runs, reported by
`list_stop_points` as `Hits: N` on every kind — **including `Hits: 0`**, printed rather than omitted, because
an absent line cannot be told apart from a build that does not count.

It counts what the *JVM reported for this stop point*, which is deliberately not what the caller was told
about. A hit whose **condition** was false counts: the line ran, and "400 hits, none matched" is a different
diagnosis from "never ran". A folded **rethrow** counts although it does not spend the trace budget, so on a
traced stop point the tally and the capture count answer different questions instead of repeating one. An exit
from a method other than the one asked for does *not* count — that is JDWP delivering traffic the request
could not filter, not the stop point firing; it is counted as a **discarded exit** instead. Charged once per
hit, never once per armed location.

_Avoid_: hit count. That name belongs to the **`hit_count` argument**, which is the opposite kind of thing — a
*requested* selector saying which single occurrence to stop on (JDWP's `Count`), after which the stop point is
**spent**. One counts what happened; the other chooses what will. The collision is not hypothetical: the two
were both called `hit_count` in the code, and the tally sat dead and always zero behind a listing that could
never report it. Fixed in FILT-10 (#110) by renaming the tally to `hits` and leaving `hit_count` to mean the
argument alone.

**Discarded exit**:
A `METHOD_EXIT` event the debuggee generated, sent and paid for, which belonged to a method other than the one
the request asked for and was dropped on this side. Counted per method-exit stop point that carries a `method`
filter, and reported by `list_stop_points` beside the **hit tally** as `exits discarded: N`.

Its own word because it is neither a **hit** nor nothing, and until TRACE-15 ([#156]) the vocabulary had only
those two. JDWP has no method-name modifier: a `ClassMatch` delivers every method of a matching class, so the
`method` filter runs *here*, after the debuggee has already been charged. That makes a discarded exit the one
kind of cost this server imposes that nothing was reporting.

**The pair is the diagnosis; neither number is one alone.** `Hits: 0` by itself cannot tell *the code never ran*
from *it fired constantly and every event was discarded* — the two are different investigations and printed the
identical line. Beside `exits discarded: 0` it means the class produced no exits at all; beside a non-zero count
it means the class is executing and the method asked for is what did not return. #156 was filed from a real
investigation where that ambiguity cost two full end-to-end runs and came close to a supplier-side bug report
for a hang that did not exist, on a request that went from 3.2 s unarmed to a 240 s read timeout armed.

**Zero is printed, and nothing is printed without a `method` filter.** The first for the same reason `Hits: 0`
is printed — the number only works as a pair. The second because a request that names no method wants every
method and discards none, so a count there would assert a filter that was never armed.

_Avoid_: dropped hit (a discarded exit is specifically **not** a hit of this stop point, which is the
distinction the two counters exist to draw), missed hit (nothing was missed — the request
was never for that method), filtered event (**filter** on this surface means a modifier the JVM applies, and
the whole point here is that this one is not)

[#156]: https://github.com/YgorPerez/java-debugging-mcp/issues/156

**Event set**:
What the debuggee actually sends: **one** packet carrying one **event** per request that matched at that
moment, plus a single suspend policy for all of them. Three stop points on one line produce one event set
with three members, not three arrivals.
Its own word because two facts belong to the set and to nothing inside it, and both were found as bugs
rather than read off the spec. **The thread is suspended once for the set**, however many members it
carries — so a resume per member undoes suspensions the hit never took (BP-6, [#102]). And **the policy is
the strongest any member asked for**: measured on Temurin 17/21/25, a set carrying one `All` request and
two `EventThread` ones arrives as `All`, which is how a **trace** stops being non-suspending — silently
until TRACE-12 ([#117], ADR-0031) made both the arm reply and the listing say so.
So "the stop point suspends the VM" is a sentence about a set, and a reader who has only the singular word
will write code that reads the first member — which is exactly what [#102] was.
_Avoid_: composite (JDWP's wire word for the packet, `Event.Composite`; fine when quoting the protocol,
wrong as this concept's name — the same rule **alert** applies to `notifications/message`), event (the
singular is a member of one, and conflating them is the defect above)

[#102]: https://github.com/YgorPerez/java-debugging-mcp/issues/102
[#117]: https://github.com/YgorPerez/java-debugging-mcp/issues/117

**Event**:
One member of an **event set**: a hit that suspended the debuggee and is reported to the caller, who is
expected to resume it. Reported **two** ways, and both always happen: recorded in a bounded buffer the caller polls, and pushed as an
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

**The word acquired a homonym in the test suite** (DOC-7, DOC-8), and it is left there on the same test that
kept `bracket` where it was. `tests/tool-descriptions.txt` and `tests/argument-schemas.txt` are *snapshots* in
the ordinary snapshot-testing sense, and **both senses are test names in the same crate** — a handful read
`…_snapshot_carries…` / `…_in_one_snapshot` and mean a captured hit, while `…_match_the_committed_snapshot`
means a committed baseline. Snapshot testing is a general testing term rather than a concept of this domain, so
it earns no entry of its own; what earns the note is that a reader who meets it and looks the word up here
finds a trace capture instead. Written down rather than renamed away, and cheap to live with because the two
never meet where it would matter: **no reply and no tool description uses the second sense**, so a caller can
only ever encounter one of them.

**Trace**:
The non-suspending mode of any stop point — snapshot the hit, resume the thread, never surface an event.
The safe mode on a shared instance, and the word this project uses throughout for it.
**It is a property of the event set, not of the stop point, and the difference is not pedantic.** A traced
stop point asks for `EventThread`; what it *gets* is whatever policy the strongest member of its **event
set** asked for. So a suspending stop point at the same location makes every traced one there suspend the
whole VM. Measured on Temurin 17/21/25 ([#117]); the sentence above holds for a stop point that does not
share its location, which is every case anyone has been in so far.
**It no longer fails silently.** Arming either way round is accepted — suspending on a line you are already
tracing is a legitimate thing to want — and the reply names the stop points whose behaviour just changed,
while `debug.list_stop_points` marks an escalated one `(trace — SUSPEND POLICY OVERRIDDEN)` rather than a
bare `(trace)` (TRACE-12, ADR-0031). Two traced stop points on one line are untouched: `EventThread` plus
`EventThread` is still `EventThread`.
The rule that follows is worth stating in the caller's terms rather than the protocol's: **on a shared
instance, keep every stop point on a given line traced.** One suspending stop point revokes the promise for
all of them.
**A second route breaks the same promise and has nothing to do with the event set** (DUMP-8, #123). A
`trace_expr` is evaluated on the hit thread while it is briefly held, and an invocation is **uncancellable**: one
that outruns its budget leaves the debuggee inside the call, and the JVM re-suspends that thread when the call
finally returns — after this side has already resumed it and moved on. Measured at 1.2 s later on Temurin
11.0.32 and 21.0.12, and the thread then stays suspended for good. An invoking `trace_expr` is refused on a
monitor stop's `blocked` kind (ADR-0036), which closes the route where the dangerous expression is also the
natural one; everywhere else it is accepted, and outside a **read-only** session nothing checks it. So the
promise is "suspends nothing *of its own accord*" rather than "suspends nothing".
_Avoid_: logpoint, tracepoint; and "non-suspending stop point" as a standalone claim, which is the
overstatement both paragraphs above are about — an escalated event set and a stalled invocation break it by
routes with nothing in common, so ruling one out says nothing about the other

**Caller chain**:
The callers above a hit, recorded on a snapshot as locations only. Answers which path reached the hit
without the suspension that reading a full stack would need.
_Avoid_: stack, backtrace (both imply the whole stack, with locals)

**Filter**:
A modifier on an event request that the *debuggee* applies, so an occurrence that does not match
produces no event at all — no packet, no suspension, no work on this side. The ones in use here are
`ThreadOnly` (one thread), `InstanceOnly` (one object), `ClassOnly` / `ClassExclude` (class patterns, used
by stepping) and `Count` (the Nth occurrence only, after which the request is **spent**).
Worth its own word because it is the only mechanism here that reduces what the debuggee *does* rather than
what the debugger reports, which is the difference that matters on **the shared 8180**. Everything else —
a **condition**, a method-name narrowing on a method-exit request, a monitor stop's `min_duration_ms`, a trace
budget — filters after the event has already crossed the wire.
**Two caller-facing names use the word for our side anyway**, and that is a stated mismatch rather than drift:
`class_filter` on `debug.get_traces` selects among records already captured, and `min_duration_ms` is
*described* as filtering — "what you READ, not what crosses the wire". Both are already-shipped caller surface,
so the glossary records the gap instead of asserting a purity the schema does not have, exactly as **Stop
point** does for `bp_`/`breakpoint_id`. The reserved sense of the noun is still the debuggee's.
Two hazards, each with its own term: a filter the debuggee accepts and does not apply is **inert**, and a
filter naming an object or thread the debuggee has collected simply stops matching, which reads as *the code
never ran*.
**The second hazard does not apply to an ARMED `InstanceOnly` filter, and the reason is a third fact about
filters that is easy to get backwards.** Measured on Temurin 17/21/25 (FILT-9, ADR-0027): an armed
`InstanceOnly` modifier holds a **strong** reference to its object, so the debuggee cannot collect what the
filter names. Isolated against four controls — nothing armed, an unfiltered breakpoint on the same method,
the filtered one, and the filtered one after a disable and after a clear — only the armed filtered case
survives a drop plus two `System.gc()`s. So the modifier is the reference, and clearing or disabling
releases it.
Which trades one hazard for another rather than removing it. While armed, the filter cannot go silently
quiet — but it is a **retention** in the debuggee, holding the object and everything it reaches for as long
as the stop point exists, which on **the shared 8180** is a cost the caller is paying and must be told
about. And the collection hazard is not gone, only displaced: it lands on the **disable → re-arm** cycle,
where the pin is released, the application drops its own last reference, and a re-arm would produce a stop
point that lists as armed and can never fire. A `ThreadOnly` filter has no equivalent — a thread is not kept
alive by being named — which is why the two are checked by different commands and reported in different
sentences.
_Avoid_: condition (ours, and paid for per hit), narrowing (vague about which side does it)

**Condition**:
A boolean expression **we** evaluate, on the hit thread, after the debuggee has already reported the hit.
That is the whole of what distinguishes it from a **filter**, and the distinction decides what each one
costs: a filter (`ThreadOnly`, `InstanceOnly`, `ClassExclude`) is a modifier the *debuggee* applies, so a
non-match costs no packet and no suspension at all, while a condition costs a hit, a hold and several round
trips **every time**, whether or not it turns out true.
So the two are not interchangeable and neither dominates. A filter is free and can only ask what the
protocol has a modifier for; a condition can ask anything the expression grammar can express — a field, a
chain, a comparison — and is paid for per hit. Reach for the filter where one exists.
What a condition costs was not always this: until FILT-7 ([#91]) a conditional stop point froze the whole
VM to decide, so the argument reached for to make a hot line *cheap* was the most expensive thing available.
It now holds only the hit thread and releases it when the answer is false — see **escalation** for the
window that opens when the answer is true, and ADR-0020 for why the policy is `EventThread`.
_Avoid_: filter (the debuggee applies those; conflating them hides that one is free per non-match and the
other is not), predicate (fine in prose, but the caller-facing argument is `condition` and the glossary
should agree with the schema)

[#91]: https://github.com/YgorPerez/java-debugging-mcp/issues/91

**Filter pin**:
The debuggee holding an object alive because an **armed** `InstanceOnly` **filter** names it. Released by
clearing or disabling the stop point, and by nothing else.
It needs a name because **there are two pins in this project and only one of them is ours**, and their two
ADRs read as a contradiction without this sentence. ADR-0022 — *"an object handle is printed weak and never
pinned"* — is about the debugger declining to pin: `ObjectReference.DisableCollection` is available, is not
used, and never will be, because the debugger must not be the reason a live heap cannot be collected. A
filter pin is the *debuggee's*, taken on its own initiative, bounded by the stop point's lifetime and
invisible in the protocol. ADR-0027 measured it; nothing chose it.
The consequence a caller pays is real either way: an armed scoped stop point retains that object **and
everything it references** on **the shared 8180**, which is why every arm reply states it rather than
leaving it to the ADR. The consequence they *gain* is that while armed, the filter cannot silently stop
matching — the debuggee cannot collect what it is holding — so the **vanished** hazard moves to the
disable-then-re-arm cycle and lives nowhere else.
_Avoid_: pinned, on its own and unqualified (the bare word is on **held thread**'s avoid list for a
different and still-correct reason — a *pinned thread* is an application state, and these are objects),
retention, leak (the first is vague about who is holding, and the second says the debuggee is at fault for
doing exactly what the protocol asks of it)

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

**`sighting` acquired a second sense outside the code, and it is left there on the `snapshot` test.** In this
entry a sighting is one throw of one exception instance at one site. In the issue tracker, in commit bodies and
in soak reports it means **one observed failure of a flaky test** — and the distinction that sense carries is
load-bearing rather than casual: a sighting is *not* a reproduction, so a flake can have many sightings and
still not be reproducible on demand, which is the whole state #118, #71, #64, #56 and #45 are in after forty
full-suite runs. #126's brief turns on it too, distinguishing "make the next sighting legible" from "explain
this one".
The two are close enough to look like one word used loosely — both are "one observation of a recurring
thing" — which is exactly why it is written down. What keeps it cheap is the same measurement the `snapshot`
note rests on: **no reply and no tool description uses the second sense.** Every caller-facing use of the word
— `debug.set_exception_stop`'s description and the fold rendering — is the rethrow one, so a caller can only
ever meet this entry's sense. The flake sense stays in the tracker and in git history, where its subject is
never an exception.

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

### Outliving the session

**Export** (verb):
To emit something in a form that outlives the session. Names exactly two artefacts — a **stop-point set** and an
**investigation report** — and means the same thing for both.
_Avoid_: save, dump (both imply this server writes the thing somewhere, and it does not)

**Stop-point set**:
The armed stop points of a session written as the list of `debug.set_*` calls that would recreate them. Content
the client stores: this server writes no file and reads none (ADR-0041).

Its own term because it is neither the live set nor a listing of one, but a *description* of one — which
survives the process the live set dies with. Under stdio the client's lifetime **is** the session's, so
tomorrow's investigation starts with no stop points at all.

**It carries what the caller asked for, never what the resolver worked out.** The line or the method they named,
not the method a line turned out to sit in. Written down because the first version had it the other way round —
a stop point armed by line came back naming its method too, which is right on the build it was exported from and
wrong on any build where that line has moved.

**An instance filter and a thread filter are both dropped**, each being a handle into one JVM (ADR-0022), which
leaves those entries **broader** than what was exported. Both of them, not just the first: the thread filter was
missed when this was designed.

**Not a `bpset_`, and the near-miss is worth stating.** That prefix abbreviates *breakpoint set* and names a
**wildcard family** — a live collection this glossary calls a family precisely because *set* was the wrong word
for it. A stop-point set is the other kind of thing: a **description** rather than a live collection, and one
that *contains* families among its entries. So the two nest rather than compete, but they are one word apart and
a reader who meets `bpset_1` inside an exported set is entitled to check which is which.

_Avoid_: profile (the upstream word; it implies storage this server does not have and names it does not keep),
session export (that is an **investigation report** — a different artefact sharing only the verb), saved
breakpoints (a set carries all five kinds plus wildcard families, and it is not necessarily saved anywhere)

**Investigation report**:
The whole session as one Markdown document — attach target, VM version, every stop point with its measured
capture cost, the snapshots the ring still holds, the staleness verdicts, the budget disarms. **Unredacted**: it
carries whatever the debuggee's variables carried, and says so before any of it (ADR-0042). What you attach to a
ticket.

Its own term because it is the **session** and not the trace buffer, which is the distinction that earns it a
name: the stop points, their costs and the VM version are what make a snapshot interpretable, and none of them
is in the buffer.

_Avoid_: session export (the noun for this is *report*, and **session** is separately loaded here — it is the
live attachment being reported on), trace dump (it is not the buffer, which is the whole point), audit log
(nothing is written continuously or kept by this server; a report is produced on request and handed over)

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

**Spent**:
A stop point the **debuggee** removed on its own, without telling anyone. Only `hit_count` produces this: JDWP's
`Count` modifier reports the Nth occurrence and then deletes the request inside the JVM, so the stop point fires
exactly once and is gone.
Its own word, and the fifth in this cluster rather than a variety of the fourth, because of the one property the
other four do not have: **a disarm is something this debugger does, so its own records are right by
construction; a spent stop point is something the debuggee did, so its records are a stale belief.** There is no
event, no reply packet and no acknowledgement — the request id we hold is one the JVM has forgotten. Two
consequences follow from that and from nothing else, which is why the distinction earns a term: such a stop point must not be
listed as armed, and clearing it must not send a clear for a **request id** the debuggee may have since reissued
to someone else. Both are now true — `list_stop_points` renders `SPENT` with its own glyph and
`clear_stop_point` sends no packet and says it did not (FILT-8, #99; ADR-0026, which also records why "always
clear and ignore the error" is wrong *here* specifically).
_Avoid_: disarmed (this cluster's word for the automatic case, and the conflation that hides the staleness —
the watchdog and a trace budget disarm are ours and known, this is neither), expired (suggests time),
consumed, retired (both already spoken for — an entity read, and a pool worker)
_Also avoid_ using **spent** for a **trace budget** running out. That is a disarm: we count it, we do it, and we
know when it happened.

**Inert**:
A **filter** the debuggee accepts and then does not apply. The request is armed, the filter is on it, and
the filter does nothing — with no error and nothing in any reply to say so.
**The capability bit is not a guide, which is the trap.** `canUseInstanceFilters` reads *true* on the JVM
where the filter is inert: the debuggee is telling the truth about what it supports and still not applying
it on the request in front of it. So a bit that says yes licenses nothing about a particular event kind.
Its own state in the cluster above, because the failure direction is the opposite of the others: **spent**
and **vanished** are the debuggee having removed something we still believe in, while this is the debuggee
keeping something that was never in effect. So the stop point reports *more* than it should rather than
less, which is the reading no caller checks for.
Measured on Temurin 17/21/25, `HotSpot` (FILT-9, #101, ADR-0027), and on 11.0.32 for the fourth (DUMP-7,
#96, ADR-0035): an `InstanceOnly` filter is accepted and **not applied** on a `METHOD_EXIT` request, on a line
stop in a `static` method, on a watch of a `static` field, and on a **monitor stop point** — four shapes, all
silent. The last was measured with a real object id against a probe whose every frame is `static`, so `this` is
null and nothing could legitimately have matched; the request armed cleanly and reported all three of its
locks. The consequence is a rule rather than a caveat: **acceptance is
not application**, so a filter must be refused up front where it is known to be inert, since neither the
reply nor the JVM will ever mention it again.
The rule cuts both ways, and the fourth shape is why it has to be measured per kind rather than reasoned
about. `METHOD_EXIT` has a `this` and is still inert; `EXCEPTION` also has one and **works** — the same
probe, two instances throwing the same type from the same line, 26 records and every one of them the
filtered instance. Neither outcome is predictable from the protocol, the capability bit or the presence of
a `this`, so each kind is one probe run and the table is the answer.
**A modifier can also be applied to the wrong SUBJECT, which is not this and has no name yet.** DUMP-7 found
`ClassOnly` accepted on all four monitor kinds and applied to the *monitor's* class on the wait pair but to the
*blocking code's* class on the contended pair (ADR-0035) — so the filter works, and narrows something other
than what the argument names. Deliberately left unnamed on one data point: **inert** earned a word by recurring
across three shapes, and one instance does not. The remedy is the same either way — refuse it where it would
mislead — so nothing turns on the missing term until it recurs.
_Avoid_: unsupported (the debuggee took it — an unsupported modifier is one it *refuses*, which is the
honest case and needs no word), ignored (true but reads as ours to fix)

**Disarming stops future hits, not hits that already exist.** A stop point can be armed and gone while hits
it caused are still unhandled — see **in-flight hit**. Treating "disarmed" as "silent" is what froze a
debuggee in #72.

**Session default**:
A value set once on `debug.attach` or `debug.launch` that later calls use when they name none of their own.
`trace_expr` is the one a caller sees named as such (EVAL-14); `source_roots` and `class_roots` are the same
shape and predate the phrase.

**It is a default and never a merge**, which is most of why it needs a word. A stop point naming its own
`trace_expr` records exactly that list, and the two are never combined — combining them would push a
caller's own four expressions past the cap and drop the tail, which is the failure the cap exists to reveal
rather than to cause.

**A reply says when it took one.** A capture nobody asked for at this site is otherwise unexplained, and
unexplained output that reads as an answer is the thing this glossary's **Reply** entry exists about.
ADR-0040 records why this is a default rather than a `watch` tool family, and why the two lists are
never combined.

_Avoid_: **inherited** — that word is already taken on this surface for a field walked from a superclass
(`list_fields {inherited:true}`, ADR-0015), and one word doing two unrelated caller-visible jobs is exactly
the defect **Reply** was written for; it was used here for a day and replaced. Also avoid *watch* and *watch
list*: no `watch_*` tool family exists, and `set_field_stop`'s watchpoints are an unrelated subject

### Suspension

**Suspended**:
Held by the debugger, which is the only state in which a thread's frames and locks can be read. Counted, so
a thread suspended twice needs two resumes.
_Avoid_: stopped, frozen, paused (all read as application state)

**Blocked**:
Stopped by the application's own logic — waiting on a monitor, sleeping, parked. Independent of suspension,
and a thread can be both: a wedged thread is blocked but not suspended, so its stack stays unreadable until
the debugger suspends it as well.

**Monitor event**:
One of the four transitions the debuggee reports around a lock: a thread beginning to wait for a monitor
another thread holds, that thread acquiring it, a thread entering `Object.wait()`, and that wait ending.
Their value is what they are *not*. **Blocked** is a state, and reading it means suspending the thread and
asking — which makes "requests are hanging on a lock" the one wedged-server question that forces a suspension
of a shared instance. These are that state's **edges**, reported as the debuggee runs, so the same question
becomes a stream to watch instead of a freeze to impose.
**They come in pairs, and only the second of each pair carries a duration** — the beginning of a wait cannot
know how long it will last. Not a detail: a "report only waits longer than N ms" filter is expressible on the
two *end* events and meaningless on the two *start* events, so offering it on all four would silently do
nothing on half of them.
_Avoid_: contention event (true of one pair, wrong for the other — `Object.wait()` is a thread waiting to be
*notified*, not competing for a lock), deadlock (a cycle in who waits for whom; these are evidence for one and
never a statement of one)

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
_Avoid_: pinned, parked, frozen (the first two are application states, the third reads as the whole VM).
The bar on *pinned* is about **threads** and is not retracted by **filter pin**, which is a different
subject — an object the debuggee holds — and is always qualified for that reason.

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
The timer that resumes a debuggee left **VM-wide** suspended too long and disarms whatever froze it, so a
forgotten suspending stop point cannot hold a shared instance indefinitely.
**It does not cover a thread held by a traced hit**, and the word "VM-wide" is the whole of the difference.
A traced hit suspends only the hit thread and never records a suspension, so nothing the watchdog reads is
set and it has no reason to look — which is why an in-flight traced hit whose request went away could leave
one application thread frozen for the life of the JVM (#114). `debug.panic` was the only escape, because it
resumes the VM outright rather than by reading who is holding it.
**It has now happened twice by unrelated routes**, which is what turns the `_Avoid_` below from a caution into
a fact. #114 was an in-flight hit whose request went away; DUMP-8 (#123) is a `trace_expr` whose invocation
outran its budget, where the JVM re-suspends the thread when the call finally returns — over a second after
this side resumed it and moved on. Neither suspension is VM-wide and neither is one a caller asked for, so the
watchdog reads nothing in either case, and the debuggee's own progress is the only thing that shows it.
_Avoid_ reading this as "nothing stays frozen": it bounds the suspension a **caller asked for**, not every
way a thread can end up held.

**Rescue**:
What the **watchdog** does on finding the **debuggee** VM-wide **suspended** past its bound: resume it *and*
**disarm** whatever froze it. Both halves, because resuming without disarming re-freezes on the next hit of the
stop point that caused it.
_Avoid_: auto-resume (it names half of the action, and the half it omits is the one SAFE-2 and SAFE-5 were
filed about)

**Resume honesty**:
The property that after **any** resume path, from **any** suspended state, the debuggee is genuinely running —
or the reply said out loud that it is not. Asserted against the **probe**'s own output rather than a return
value, because every tool here reports success either way (ADR-0003).

Its own term because the failure it names is not "the resume failed" but "the resume reported success while the
VM stayed stopped", and those are different things needing different words. The first is visible. The second is
what five review rounds kept shipping: every round's worst bug was in the previous round's safety work, and the
watchdog was wrong three times.

**Disarm honesty is the other half, and deliberately not this word.** A VM that resumes and is then immediately
re-frozen by a still-armed stop point was resumed honestly and **rescued** dishonestly — the SAFE-2/SAFE-5
harm. The two are asserted differently: resume honesty asks whether the probe ticks at all, disarm honesty asks
at what *rate* it ticks after a rescue. That is why they are not one test, and why they are not one word.
_Avoid_: resume success (that is the return value, which is the thing being distrusted), verified resume,
liveness (it is a claim about what the reply *said*, not only about the VM)

### Identity

**Object handle**:
The caller-facing name for one object in the debuggee — `@0x1f4c`, a JDWP `objectID` in hex. An expression
**head** and only ever a head, so `something.@0x1f4c` is meaningless and is refused as such.
It is defined here because three other entries already lean on it — **head**, **bound head** and **copy** —
and because a convention nobody wrote down is one that drifts, which ADR-0022 names this file as the place to
prevent. The rule it exists to keep is the one **loaded** records from SIG-1: *a name this tool shows is a
name it accepts*. A rendered object ends in exactly this string, so a handle read out of a trace snapshot, a
deep render or `debug.list_instances` pastes straight back in.
**The classloader selector is the same rule, not a second one.** `com.example.Utils@0x7f3a1c` pins which
**copy** a read resolves against by suffixing the class name with *the loader's* `objectID` — so both forms
are "`@0x` + a JDWP objectID, in hex, copied out of a reply". **Position** disambiguates them: a token that
*starts* with `@` is a handle, an interior `@` belongs to the class-name path. The two compose deliberately
(ADR-0019, ADR-0022) rather than colliding, and a selector matching no loaded copy is an error naming the
loaders that do exist — never a quiet fall back to the first.
Whether the debuggee can collect the object out from under a handle is **filter pin**'s subject, not this
one's.
_Avoid_: **handle** for a **stop-point id** or a **wildcard family** — those are *ids*, and this entry is why
the word is now spent; address, pointer (a JDWP id is neither, and it is not stable across a redeploy);
reference (the Java word for what the debuggee holds, not for what the caller types)

**Stop-point id**:
The caller-facing id for a stop point (`bp_1`, `exc_2`, `watch_modify_3`, `mexit_4`). Stable for the
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
**A JPA query is the case where the destructive read CAN be prevented, which is why it is not just another
example** (EVAL-11, #124). JPA's default flush mode is `AUTO`, under which the provider pushes every pending
change in the persistence context to the **database** before answering a query — so asking would commit
somebody else's half-finished work. `debug.run_named_query` sets `FlushModeType.COMMIT` on the `Query` it
created, which suppresses that for one query and touches neither the `EntityManager` nor anyone else's, and
it **refuses to run at all** when it cannot (a bean implementing neither JPA API, an unloaded
`FlushModeType`) unless `allow_flush:true` says the write is wanted. The difference from the single-pass
stream above is the remedy: nothing can stop `readEntity` consuming a body, while this needed one setter and
a reply that states the cost of having used it — the rows no longer reflect uncommitted changes

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

**Its existence and its contents answer different questions, and TEST-39 is what happens when a test confuses
them.** That a tick appeared says the debuggee is still running; the figures *on* it say what state it is in.
`a_thread_filter_holds_against_a_real_pool_of_reused_threads` needs a saturated pool for its premise to mean
anything — it waited for *a* tick and then asked the **debugger** how many workers there were, getting 85,
because `prestartAllCoreThreads()` had not finished starting 200 of them. The probe had been printing
`pool=<size>` on every line the whole time.
So: **where a probe announces the state a test's premise depends on, the test reads it from there.** The
debugger's answer is a second opinion that can only agree or disagree with the debuggee, and a premise checked
against the wrong side fails as an assertion about something else entirely.
This is the test-side half of a rule the probes already keep on their own — `ContendedProbe`, `MonitorProbe`,
`SyntheticProbe` and `WedgeProbe` each ask the JVM for a thread's real state rather than spinning on a flag,
and each says so at the point of the wait.

**Witness**:
A probe's standing as independent evidence — it prints the thing under test, so a test can distinguish what
the debuggee did from what the debugger claims. A probe **stops being a witness** when it stops printing,
and the word exists because that state is silent and reads as a debugger bug.
Three separate flakes were spent on this before it had a name (TEST-31/#114, TEST-33): a worker that caught
an exception and returned; a worker frozen by a held hit; a stop point armed on a method nothing was
calling. Every one presented as *the debugger armed something and it never fired*, and none of them was
that. The rule the term is for: **a probe must announce that it has stopped**, because the absence of a
**tick** is the same absence whatever caused it, and only the probe can say which.
Its counterpart is under **tick**, and TEST-39 cost a flake for want of it: announcing is only half the
bargain, and what a probe announces has to be *read* rather than inferred from the fact that it spoke.
_Avoid_: "the probe is alive" — a live thread that no longer executes the code under test is exactly the
case that misled three investigations.
Its other counterpart is **separated** below: a witness only helps if the assertion that reads it says which
part of the evidence failed.

**Separated** (of an assertion):
A failing assertion whose message names **which** of its candidate causes fired, rather than one it could not
have established.
**It earns a name because in a debugger every observation has at least three candidate causes that present
identically** — the product is wrong, the **probe** stopped, or the test raced — so a message that picks one is
right by luck. Three fixes here are the same fix: #118's was split into *class never prepared* / *prepared but
the arm did not land* / *armed and never hit*; TEST-40's (#125) into *never ticked* / *ticked then stopped*,
which had been blaming a probe for stopping when it had never started, because its "before" reading was a
default on every run since the test was written; and #126 asks for *stranded* against *transient*. Each cost a
flake investigation for want of the distinction, and one of them cost two.
**It is not ADR-0034's rule, and the two are complementary.** That rule — *a negative assertion has to be seen
failing before it is trusted* — establishes that an assertion **can** fire. This asks what its firing then
*means*: an assertion can be seen failing, fire correctly, and still name the wrong cause. Worth knowing where
that rule lives, because nothing in the ADR titles points at it: it is in **ADR-0034**, whose subject is
conditions naming what a hit carries, recorded as a lesson from the implementation rather than as the decision.
_Avoid_: "a clear error message" (says nothing about causes), "specific" (a message can be specific and
confidently wrong, which is the failure mode rather than the fix), flaky (a property of a test's outcome, not
of what its message can establish)

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
A recorded JDWP session — every command and the reply packet it got — kept in a file and served back to the
debugger with no JVM behind the port. One debuggee on one JVM, fixed at the moment of recording, so it
complements a probe rather than replacing one: it cannot notice the debuggee changing, and it can be *edited*
into a shape no JVM here could be asked to produce. See ADR-0014.
(This read "a **snapshot** of one debuggee" until the homonym under that term was written down. Neither sense
was meant — a cassette is not a captured hit and not a committed baseline — which is the argument for the note
rather than against it.)
_Avoid_: mock, stub, fixture (the first two suggest something written to satisfy the test; a cassette is a
transcript of a real session, and its authority comes from that)

**Miss**:
A request a cassette has no recorded answer for. Never answered — the connection is dropped and the command
is named — because a plausible-looking error reply packet would let a replay test pass while proving nothing.
