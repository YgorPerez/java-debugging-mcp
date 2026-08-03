# ADR-0032 — An unfetched Hibernate association is a third answer, and the cheap gate is a name

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** EVAL-9 ([#86](https://github.com/YgorPerez/java-debugging-mcp/issues/86))

## Context

`debug.evaluate_chain` is the right tool for this stack's dominant bug shape and it **invoked** each link.
Measured in `infotravel`: **1897 `FetchType.LAZY` associations and zero `EAGER`** across 694 `@Entity`
classes, one `Hibernate.isInitialized` call in the whole tree, and 30 `catch (LazyInitializationException)`
sites — eight of which name the caught variable `PORRA`. Chains run to seven links.

So walking a chain did one of two things, both of them changes to the debuggee:

1. **Threw `LazyInitializationException`** when the entity is detached, which 471
   `@TransactionAttribute(NOT_SUPPORTED)` sites make ordinary rather than exotic. The chain report then
   blames a link that is fine.
2. **Silently issued SELECTs into a live persistence context** — on the shared 8180, someone else's
   in-flight request whose entity graph the "read-only" diagnostic had just mutated.

Verification against a real detached proxy (hibernate-core 5.4.25, `ByteBuddyProxyFactory` with a null
session, Temurin 21) found a **third** case the brief did not name, and it is the worst of them: a **field**
read on an uninitialised proxy returns the proxy's own inherited copy, which is never populated. `proxy.id`
read `null` through this debugger while the proxy's identity was `42`. That is a wrong answer with **no
error at all**, where the invoke at least throws.

## Decision

### An unfetched association is a third answer, not a null and not a value

`⏳ UNFETCHED` sits alongside "a value" and "`null`". A chain walk stops there and says which of the three it
is; `ChainStep` carries three endings rather than a `bool`. Folding it into `null` would report "this link is
null" about a row that very likely exists, which is the chain report blaming the wrong link — the exact
failure the tool was built to remove. `check_stale` already models a **cannot tell** third answer
(ADR-0030), and a fourth state exists here for the same reason: it *is* a lazy value and the flag could not
be read.

### The marker interface decides; the class name is only a cost gate

Two stages, and the split is a measurement rather than a preference.

The **interface** is the decision — `org.hibernate.proxy.HibernateProxy`, or
`org.hibernate.collection[.spi].PersistentCollection` — because a generated class name is a library naming
strategy while the interface is API.

The **name** gates whether the interface is asked at all, because asking unconditionally cost too much:
running the interface check on every link of every expression took a 5-link chain against a probe with **no
Hibernate anywhere from 34 JDWP packets to 49 (+44%)**, reproducibly. A lattice walk returns `false` only
after visiting the whole lattice. #86 requires that non-proxy chains behave as they did before, and a 44%
round-trip tax on every chain in every JVM is not that. With the gate the same chain costs **34 packets —
identical to the baseline, first walk and second**.

**The gap is stated rather than left to be found:** a deployment configuring `hibernate.proxy.factory_class`
with a factory using some other naming strategy would not be a candidate, and its proxies would be walked
into as before. There are no false positives — the interface is still authoritative for everything that is a
candidate — only that one shape of false negative, and stock Hibernate does not produce it.

A per-link classification in the *renderer* was measured and removed for the same reason: it cost +4 packets
on a 5-link chain because the object-to-type lookup is per **object** and cannot be cached. It now runs once
per walk, on the trailing link, which is the only one `resolve_member` never sees as a receiver.

### Which reads are refused differs between the two shapes, and both exemptions were measured

The check sits in `resolve_member`, above the field/method split, because on an entity proxy **both** are
wrong. But not every read is:

- On an **entity proxy**, a field the proxy class *itself declares* is the proxy's own state.
  `$$_hibernate_interceptor` is set at construction and is exactly what the detection reads. Only an
  **inherited** field hands back the unpopulated copy; only a method is always intercepted.
- On a **persistent collection**, every field read is safe. The collection is nobody's stand-in, its fields
  *are* its state, and it is `size()`/`iterator()` that run the deferred SELECT.

Both exemptions came from running the first implementation against real Hibernate, where it refused reads
that trigger nothing — including the debugger's own diagnostic one, so the single expression that could
confirm the verdict was the one it would not answer.

### `force_initialize` is the opt-in, and read-only refuses it by name

The load is a **write**: it runs Hibernate's deferred SELECTs inside the debuggee. So `read_only` refuses
`force_initialize` at the argument rather than letting the invoke fail with a message about something else,
and the *report* still works in a read-only session — it needs no write, which is why it is the default.

Only `debug.evaluate` and `debug.evaluate_chain` offer it. Conditions, `[?pred]` filters, `trace_expr` and
`set_value` sources are always `Report`: a diagnostic that quietly loads a lazy association **on every hit**
is the read-only-looking tool that changes the debuggee, at its worst in trace mode where it fires hundreds
of times.

### Rendering an unfetched value must not `toString()` it

On a proxy that call *is* the load. `render_object` reports the third answer instead — and at no extra cost,
because it already has the type in hand.

## Consequences

A caller reading a chain now sees `⏳` where they used to see a value that had been fetched for them, or a
`null` that was really "nobody looked". That is caller-visible and pinned downstream
(`docs/toolkit-contract.md`), so the release notes state it.

The names are **not** proved by the test suite and cannot be: the suite must not depend on hibernate-core
being installed, so `LazyProxyProbe` and `LazyCollectionProbe` reproduce the shape structurally and each says
so at the top of its source. The names were measured separately — `javap` against hibernate-core
**3.5.6-Final, 4.3.1.Final and 5.4.25.Final** (`initialized` is `private boolean` on
`AbstractLazyInitializer` and on `AbstractPersistentCollection` in all three), and a real proxy through this
debugger, which read `initialized = false` by field reads alone and then watched `force_initialize:true`
throw `LazyInitializationException`. #86 records the table.

The structural probes are also the first here to declare a **package**, because the thing under test is a
fully-qualified type name. `Probe::launch_in_package` exists for that and for nothing else.

## Alternatives considered

**Invoke `isUninitialized()` / `Hibernate.isInitialized()`.** One invoke to find out whether invoking is
safe, on the thread whose state is in question, and refused by `read_only` — so the check would be
unavailable exactly where it matters most.

**Detect by class name alone.** Cheapest, and wrong in the other direction: it would report an unfetched row
for any class somebody happened to name `$HibernateProxy$`.

**Probe once per session whether Hibernate is loaded.** Correct and cheap, but a cached negative goes stale
the moment Hibernate initialises, and re-probing per call costs a packet per link — more than the name gate
and no more reliable.

**Fail open when the flag cannot be read.** Rejected outright by the issue, and rightly: it would perform the
side effect the check exists to prevent, in precisely the case where something unexpected is going on.
