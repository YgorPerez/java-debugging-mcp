# ADR-0037 — A named query suppresses its own flush, and its `EntityManager` is found in the frame or not at all

**Status:** Accepted
**Date:** 2026-08-04
**Issue:** EVAL-11 ([#124](https://github.com/YgorPerez/java-debugging-mcp/issues/124))

## Context

`debug.run_named_query` runs a `@NamedQuery` through the application's own `EntityManager`. The question it
exists for is whether a query returns what its author believes, and the shape #124 was filed about is a
lookup whose parameters are all optional and null-guarded — `(:codigo is null or r.codigo = :codigo)` — which
matches the whole table when they arrive null, so a call meant to find one row returns thousands and the
caller takes the first. Rebuilding that predicate in a SQL client cannot reproduce it: the persistence
context, the binding and the resolved tenant are all lost.

Two things about it were decided against the brief rather than from it, and both were measurements.

## Decision

### The flush is suppressed by default, because the default JPA behaviour makes a read a write

#124's acceptance criteria say "read-only: the tool never flushes or commits". **That is not achievable by
restraint**, which is the fact that shaped the tool. JPA's default flush mode is `FlushModeType.AUTO`, under
which the provider pushes every pending change in the persistence context to the **database** before
answering a query. `getResultList()` is the write. A tool that simply avoided calling `flush()` would still
have committed somebody else's half-finished work on a shared instance, and its reply would have said
nothing about it.

So the tool sets `FlushModeType.COMMIT` on the `Query` it created. That suppresses the flush for one query,
touches neither the `EntityManager` nor anyone else's, and needs no cooperation from the application.

**The trade is in every reply rather than in the documentation**: with the flush suppressed the rows do not
reflect uncommitted changes sitting in that persistence context, so a row just saved and not committed will
not be found. `allow_flush:true` asks for the other reading and is a write, which is why the reply marks it
with a warning rather than a lock.

**It refuses rather than proceeding quietly when it cannot keep the promise** — a bean implementing neither
JPA API (so there is no `FlushModeType` to name), an enum that is not loaded, a provider `Query` with no
`setFlushMode`. A reply that omitted the note would read as a read. This is `CONTEXT.md`'s **Read-only**
entry gaining its first case where the destructive read *can* be prevented; the entry says so, beside the
single-pass stream where nothing can.

`read_only` on the session refuses the whole tool, separately and for a different reason: running the query
invokes, and it also reaches the database, which no guard here can undo.

### The `EntityManager` is found in the frame, and there is no heap fallback

#124 says the tool depends on #84 — "reaching a container-managed bean by type" — which shipped as
`debug.list_instances`. The obvious reading is that this tool calls it. **Measured against `JpaProbe` on
Temurin 11.0.32, that cannot work:**

```text
debug.list_instances jakarta.persistence.EntityManager          → 0 live instance(s)
debug.list_instances jakarta.persistence.JpaProbe$ProbeEntityManager → 1 live instance(s)
```

`ReferenceType.Instances` answers about an object's **exact runtime class**, which
`debug.list_instances`' own description already states — so asking it for the `EntityManager` *interface*
returns 0 however many beans are alive. JDWP publishes no "which classes implement this interface" command,
so there is nothing to walk.

What is left is the suspended frame, and it is enough for the common cases: `this`'s declared fields first
(the DAO/repository shape), then the frame's in-scope locals and arguments. Each candidate is tested by the
**interface it implements** — `jakarta.persistence.EntityManager` or `javax.persistence.EntityManager`, both,
because the target stack straddles the Jakarta split — rather than by its type name, since a
container-managed bean's runtime type is a proxy nobody can predict. It costs a handful of packets, suspends
nothing and invokes nothing.

When the frame has none, **the tool refuses and names the two-step precisely**: `debug.list_instances` on the
concrete implementation class, then the `@0x…` handle it prints passed as `entity_manager`. A refusal that
hands over a working route is worth more than a fallback that guesses.

### Each row is read invoke-free

#124's third criterion — "result rendering is bounded and does not initialise lazy associations beyond the
bound" — **cannot be met by bounding a depth**, because the first level is already the hazard. A shallow
render calls `toString()`, which on a JPA entity routinely names its associations; the deep one invokes
`toArray()`/`entrySet()` on a collection field. So the row read is bespoke: `ObjectReference.GetValues` over
the declared and inherited fields, rendered with no thread, which is what stops `render_value` reaching for
`toString()`. A nested value is its type plus an `@0x…` handle, which `debug.evaluate` accepts — fetching it
stays the caller's decision.

`CONTEXT.md` carries the property as **Invoke-free**, and the term was *not* the obvious one. #124's own
wording is "a bounded projection of the results", but **JPA already owns `projection`** for selecting a subset
of columns (`select r.codigo, r.status`) — and this tool's central promise is that it runs the query as
written, so borrowing that word would suggest the opposite. Naming the property rather than the artefact also
turned out to unify three hazards found separately: an unfetched association (ADR-0032), a single-pass stream
consumed by reading it (the **Read-only** entry), and an invocation wedged on a monitor its thread does not
own (ADR-0036) are all ruled out by the same thing, and a trace snapshot's locals were already read this way
with no name for it.

### The count is the true one; two different knobs bound two different costs

`max_rows` bounds what is **rendered** and never the count, because the over-match this tool exists for is a
number. `max_fetch` bounds what the **debuggee builds** (`setMaxResults`) — the real cost, since a query
matching a whole table constructs every one of those entities in its heap first — and with it in force the
reported count becomes a **floor**, which the reply says where the number is rather than in a footnote.
`max_fetch: 0` is refused: it would report 0 rows whatever the query matches, which is the one answer this
tool must never give by accident.

## Consequences

A new caller-visible tool, so the toolkit contract, the tool description, the argument-schema snapshot and a
minor version bump all apply (`docs/toolkit-contract.md`).

The reply always names which discovery route answered and what each parameter was bound as. The second is not
decoration: JPA binds by object and compares with `equals`, so a `Long` id column given an `Integer` matches
nothing with no exception and no warning — an empty result that reads like a fact about the data. Whole JSON
numbers therefore bind as `Long` and the reply shows the choice, with `parameter_expressions` taking the full
`debug.evaluate` grammar for anything JSON cannot spell.

The query text is reported best-effort through `getQueryString()`, which is `org.hibernate.query.Query`'s and
not JPA's — the spec publishes no way to read a query back — so its absence is normal and said plainly. It is
labelled **JPQL as written, not the generated SQL**, because calling it the SQL would be a small lie about
the one line a caller would act on. #124's "surface the resolved SQL if available" is answered by "it is not
available", not by relabelling something else.

**As with ADR-0032, the JPA behaviour is not proved by the suite and cannot be**: the suite must not depend
on hibernate-core, a JPA API jar or a database — `javac_into_memory` runs `javac` with no `-cp` at all — so
`JpaProbe` reproduces the shape structurally and says so at the top of its source. What *is* real is the API
surface: every name and signature it declares is the spec's, and it declares them at their real
fully-qualified names via `Probe::launch_in_package`, the mechanism ADR-0032's probes introduced.

The two criteria that no reply could evidence are checked against the probe's **own counters**: `flushes`
stays 0 only if `COMMIT` was really set, and `associationTouches` stays 0 only if nothing was invoked on a
row. Both are paired with a **positive control** that walks in deliberately and asserts the counter moves —
without it a dead counter would read as a passing assertion for ever, which is ADR-0034's lesson.

## Alternatives considered

**Vendor Hibernate + H2 into the repo so the criteria mean what they say.** The most honest test, and
rejected on cost: ~10MB of jars in git, a `-cp` threaded through `javac_into_memory` and every probe launch,
and a provenance question. The gap is stated instead — real-Hibernate behaviour is a caller-verified claim,
and the probe's header says which of its facts are the spec's and which are stand-ins.

**Enumerate every loaded class and ask each whether it implements `EntityManager`.** Exact, and it needs no
suspend, so its cost is debugger-side latency rather than a debuggee pause. Rejected because that cost cannot
be stated: roughly two packets per loaded class is fast against a probe and unknown against a
20,000-class application server over a real wire, and a tool here does not ship a cost it cannot report.

**Heap-walk a built-in list of known implementation class names** (`org.hibernate.internal.SessionImpl`,
`org.jboss.as.jpa.container.TransactionScopedEntityManager`, …). One pause, cost reportable — and the names
are unverifiable on any box without those libraries, which is the trap ADR-0032 and `LazyProxyProbe` document
at length. They are named in the *refusal message* instead, where being wrong costs a caller nothing.

**Require an explicit `entity_manager` always, with no discovery.** Zero magic, and it makes the one-call
question in the issue a two-call one even when the bean is sitting in the frame. Discovery is free when it
works, so the refusal is reserved for when it does not.

**Report the count via a wrapped `select count(*)` instead of materialising rows.** Cheaper on the debuggee
and it would change the query, which is the one thing this tool must not do — the whole point is what *that*
query returns. `max_fetch` is the honest lever, and it makes the count a floor and says so.
