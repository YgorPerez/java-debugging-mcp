# 0049 — A render reads through a closed pair of adapters, and the fixture is written rather than recorded

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
two adapters: the live connection, and a fixture written as data.** It lives in `mcp-server/src/reads.rs`
as `Reads`, and the data side is `Fixture` / `FixtureClass`.

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

The fixture has no wire and therefore nothing to guard.

### Fixtures are written, not recorded

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

### The fixture counts what it served

`Fixture::reads()` is the fixture twin of `JdwpConnection::packets_sent()`, for the same reason SAFE-9
asserts on packets sent rather than on a returned error: *refused* and *sent nothing* are different
claims, and so are *rendered correctly* and *asked for the right things*. It makes a traffic-shape claim
assertable without a socket.

## Rejected alternatives

**A trait with two impls.** The obvious shape. `async fn` in traits is not dyn-compatible, so this becomes
a generic parameter on 34 renderers and everything that calls them, or `#[async_trait]` and a box per
call. Both are more machinery than two adapters justify, and neither gives the closed-set property above.

**Widening the cassette seam to answer typed reads.** Would reuse existing machinery. It changes what a
cassette *is* — ADR-0014's whole decision is that a cassette is keyed by the request and answers in bytes,
and its miss behaviour, its record mode and its four checked-in fixtures all rest on that. A typed
answer would be a second mechanism sharing a name with the first.

**Making `JdwpConnection` itself fakeable** (a second constructor, an inner enum over "socket" and "data").
It puts a test fixture inside the type that holds the read-only guard and the event loop, which is exactly
the "give a test fixture a production job" that ADR-0001's CLEAN-2 amendment declined for `send_command`.

**Recording the fixtures from a probe.** See above: the shapes worth testing are the ones you state, and a
recording of a real class carries everything about that class rather than the one property under test.

## Consequences

- **A renderer under this seam can be tested with no JDK**, which is what makes a green run mean something
  on a box with none — the documented trap where every test prints `SKIP` and passes bites in fewer
  places.
- **The interface is small enough to read in one screen**, which is what makes "narrow" checkable rather
  than claimed. It is meant to stay that way: a renderer that needs something outside the set is evidence
  it is not one of the 30, not a reason to widen the set.
- **Two adapters means two things to keep honest.** The fixture answers what a debuggee would answer, and
  where it cannot the difference is stated at the method — `get_source_file` on a class with no
  `SourceFile` attribute is an `ABSENT_INFORMATION` *error* from a real JVM, and a test that needs that
  shape keeps its probe.
- **This is a partial migration and says so.** CLEAN-7 converted the discovery listings; the stale-bytecode
  verdicts, the source-drift verdicts and the **unfetched** report are the same shape and are not done. A
  silent partial result reads as completeness, so the count is stated in the commit that moves it.
- **`cargo-semver-checks` sees another module**, as it does everything else under `jdwp-mcp`'s
  `#[doc(hidden)]` library (CLEAN-3, ADR-0044). Reported, not failed, and not a supported surface.
