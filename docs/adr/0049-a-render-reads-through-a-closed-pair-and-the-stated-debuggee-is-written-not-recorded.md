# 0049 — A render reads through a closed pair of adapters, and the stated debuggee is written rather than recorded

## Context

`JdwpConnection` is a concrete struct with one constructor, and that constructor opens a `TcpStream`.
There is no trait, no second constructor, and the event loop is concrete in the socket's owned halves. So
178 functions in `handlers.rs` take a live connection, and 34 of the 118 reply renderers are among them —
they read class and member metadata off the wire mid-render.

The measured consequence, from CLEAN-7 ([#190](https://github.com/YgorPerez/java-debugging-mcp/issues/190)):
**30 `#[ignore]`d tests launch a probe JVM, attach, ask a question and assert on a string, without ever
making the debuggee do anything.** No stop point is armed, nothing steps, nothing resumes. They pay a
`javac`, a JVM launch and a listen wait to check how a signature renders.

What that costs is not only seconds. It decides what gets tested at all: `generics.rs` is 318 lines of
recursive descent with 7 unit tests and, until this change, **no test of the rendered listing** — because
reaching a listing meant launching a JVM. `classfile.rs` is 272 lines of hostile-input parsing with 2.

ADR-0014's cassette seam cannot reach these. It keys a cassette by the request and replays framed JDWP
bytes; a test that wants "a class with these two methods" would have to spell that in hex, and would get
back bytes rather than a typed answer. That is the right seam for what ADR-0014 does — proving the whole
stack against a recorded session — and the wrong altitude for a rendering test. ADR-0014's own rule was
*a third user is the point to unify, not the second*; these 30 tests are the third user, and the seam they
need is **above** the one it chose rather than instead of it.

## Decision

**A narrow, invoke-free read interface under the renderers and above the connection, satisfied by exactly
two adapters: the live connection, and a **stated debuggee** written as data.** It lives in
`mcp-server/src/reads.rs` as `Reads`, and the data side is `StatedDebuggee` / `StatedClass` /
`StatedObject`.

**The word is `stated`, not `stated debuggee`, and that is a decision rather than a preference.** `CONTEXT.md`
lists `stated debuggee` in the `_Avoid_` line of its **Cassette** entry, and the tree already used it for six
unrelated things — a cassette, a Java probe, the SMAP text, the Python scripts' input/output matrix, and
twice as a generic test double. Naming a type that made seven, which is the `inherited` defect `CLAUDE.md`
records, caught while the rename was still free. `CONTEXT.md` now carries **Stated** as the counterpart to
**Cassette**.

### Invoke-free reads, and nothing else

The interface carries the class and member metadata a render needs: signature, methods, fields,
superclass, classloader, the reference type of an object, source file. No mutation, no invocation, no
event subscription, and nothing that runs debuggee code.

**That boundary is load-bearing rather than tidy.** ADR-0001 puts read-only enforcement on the nine
mutating primitives of `JdwpConnection`; a second path to any of them would be a second path past the
guard. A render that calls `toString()` in the debuggee is therefore not one of these, keeps its
connection, and keeps its JVM test.

### A closed enum, not a trait

Two adapters is exactly two, so nothing needs open extension.

An `async fn` in a trait is not dyn-compatible, so a trait means either a generic parameter threaded
through all 34 renderers or a boxing dance at every call. An enum with inherent `async fn`s has neither
problem.

The deciding reason is not ergonomics, though. **A closed type keeps the set of adapters a question this
crate answers.** Adding a third is an edit to a file inside the crate that holds the read-only guard, in
front of whoever reviews it — not an `impl` anybody can write anywhere. That is the property that keeps
ADR-0001 intact as this seam grows.

### The guard does not move, and neither does `WIRE_COMMANDS`' authority

`guard_mutation` and the mutating primitives stay exactly where ADR-0001 put them. Every call on the live
adapter forwards to a command already classified `Read` by `WIRE_COMMANDS` in `connection.rs`'s test
module, and **this seam adds no `CommandPacket::new` call site**. SAFE-12's source scan reads those call
sites; leaving them untouched is what keeps it as authoritative as it was, and is why no fourth verdict is
needed (SAFE-12 records why a fourth is not the way to make a red go away).

The stated debuggee has no wire and therefore nothing to guard.

### A stated debuggee is written, not recorded

They are stated as data — a class with these methods, these fields, this superclass — and reviewed as
data. Building a second recording pipeline is the mistake ADR-0014 already argued against in its own
rejected alternatives, and a recording could not answer the question these tests ask anyway: the point is
to state a shape (a member with a generic signature, a member without one, a two-deep superclass chain)
and see how it renders, which is a shape you *write*.

### Twins where fidelity matters, replacement where it does not

The `disc2_method_listing` / `disc5_field_listing` split is the model and ADR-0014 states it as design:
one body of assertions, called by a probe test that would notice a real JVM disagreeing and by a fast test
that runs everywhere.

The rule this ADR adds: **use a twin wherever the JVM's answer is the subject, and replace outright only
where the subject is purely how the answer renders.** Speed must not quietly replace fidelity, so the
probe tests are not deleted when a fast test arrives beside them.

### The stated debuggee counts what it served

`StatedDebuggee::reads()` is the stated debuggee twin of `JdwpConnection::packets_sent()`, for the same reason SAFE-9
asserts on packets sent rather than on a returned error: *refused* and *sent nothing* are different
claims, and so are *rendered correctly* and *asked for the right things*. It makes a traffic-shape claim
assertable without a socket.

## Rejected alternatives

**A trait with two impls.** The obvious shape. `async fn` in traits is not dyn-compatible, so this becomes
a generic parameter on 34 renderers and everything that calls them, or `#[async_trait]` and a box per
call. Both are more machinery than two adapters justify, and neither gives the closed-set property above.

**Widening the cassette seam to answer typed reads.** Would reuse existing machinery. It changes what a
cassette *is* — ADR-0014's whole decision is that a cassette is keyed by the request and answers in bytes,
and its miss behaviour, its record mode and its four checked-in stated debuggees all rest on that. A typed
answer would be a second mechanism sharing a name with the first.

**Making `JdwpConnection` itself fakeable** (a second constructor, an inner enum over "socket" and "data").
It puts a test stated debuggee inside the type that holds the read-only guard and the event loop, which is exactly
the "give a test stated debuggee a production job" that ADR-0001's CLEAN-2 amendment declined for `send_command`.

**Recording the stated debuggees from a probe.** See above: the shapes worth testing are the ones you state, and a
recording of a real class carries everything about that class rather than the one property under test.

## Consequences

- **A renderer under this seam can be tested with no JDK**, which is what makes a green run mean something
  on a box with none — the documented trap where every test prints `SKIP` and passes bites in fewer
  places.
- **The interface is small enough to read in one screen**, which is what makes "narrow" checkable rather
  than claimed. It is meant to stay that way: a renderer that needs something outside the set is evidence
  it is not one of the 30, not a reason to widen the set.
- **Two adapters means two things to keep honest.** The stated debuggee answers what a debuggee would answer, and
  where it cannot the difference is stated at the method — `get_source_file` on a class with no
  `SourceFile` attribute is an `ABSENT_INFORMATION` *error* from a real JVM, and a test that needs that
  shape keeps its probe.
- **The count is stated in the commit that moves it**, because a silent partial result reads as
  completeness. Converted: the discovery listings (DISC-2/DISC-5), the **source drift** verdicts (DISC-7's
  five could-not-check branches), the **stale bytecode** verdicts (DISC-7/DISC-9/DISC-13) and the
  **unfetched** classification (ADR-0032).

## What resisted, and what that says about the scope

#190 asked that anything resisting be **reported rather than forced**, as evidence about the interface. Two
things did, and they resist for the same reason: their subject is not how an answer renders.

**The two round-trip-cost assertions cannot move, and should not.**
`a_wide_result_set_costs_a_bounded_number_of_round_trips_per_row` and
`a_realistic_rows_string_and_association_reads_share_a_round_trip` drive a probe through `LatencyRelay` and
measure **milliseconds of wire time** at an injected RTT. A stated debuggee has no wire, so there is nothing to
measure — PERF-1's claim is that independent reads overlap *on a real socket*, which is a property of the
socket and the event loop rather than of a renderer. `StatedDebuggee::reads()` answers the neighbouring question
(how many reads were asked for) and deliberately not this one. These are the clearest case of ADR-0014's
rule that a cassette complements the probe suite and must not replace it.

**A literal shared assertion body across the two adapters needs a seam this one does not own.** ADR-0014's
twin — `disc2_method_listing` called by a probe test and a cassette test — works because both adapters
produce a *tool reply*, through `Server`. A stated debuggee cannot: `debug.list_methods` reaches its renderer
through `DebugSession::connection`, so a tool call backed by a stated debuggee would mean a **session** backed by a stated debuggee.
That is `connection`'s seam, which CLEAN-6 (#189) explicitly reserves and #190 puts out of scope. So the
fast tests assert the same claims as their probe twins from beside them rather than through one body, and
can drift from them. Closing that is a decision about where a session's connection comes from, not about
this interface, and it should be taken as one.
- **`cargo-semver-checks` sees another module**, as it does everything else under `jdwp-mcp`'s
  `#[doc(hidden)]` library (CLEAN-3, ADR-0044). Reported, not failed, and not a supported surface.
