# 0017 — An exception's message is read as a field, except the one the JVM never stores

## Context

EXC-2 ([#67](https://github.com/YgorPerez/java-debugging-mcp/issues/67)) asked for the thrown exception's
**message** in a hit snapshot, and named its mechanism outright:

> `Throwable.detailMessage` is a plain `String` field, readable the same way a watchpoint reads its
> old/new values — **no invocation**, and therefore available in trace mode too. That is the mechanism this
> issue asks for.

That premise is what made the issue small, and it rests on a house discipline recorded in
`describe_method_exit_event`'s doc comment and applied by every event describer: **nothing is invoked in the
debuggee while it sits inside an event.** The describers pass `thread: None` for exactly this reason.
ADR-0006 is the same rule from the other side — object expansion is opt-in *because* expanding invokes code.
The concrete hazard is a `toString()` that blocks on a monitor another suspended thread holds; the evaluate
tool description already documents the 2-second bound that exists because of it.

**The issue's headline example is the one case its own mechanism cannot deliver.** Measured on JDK 21 with
the reflection probe in the scratch notes for this change:

```
BEFORE detailMessage = null
getMessage()         = Cannot invoke "java.lang.Integer.intValue()" because the return value of
                       "NpeShape$Detail.getCount()" is null
AFTER  detailMessage = null
explicit detailMessage = explicit text
```

JEP 358's helpful message is **computed on demand and cached nowhere**:
`NullPointerException.getMessage()` calls a private native `getExtendedNPEMessage()`, and `detailMessage`
reads null before and after. An exception constructed *with* a message stores it, so the field read covers
everything except the sentence #67 was filed to surface.

So the choice is not "field read or invocation". It is "field read, and accept that the motivating case
still reports nothing" versus "field read plus one narrowly-gated invocation".

## Decision

**Both, in that order.** `detailMessage` is read as a field for every exception. When it is null **and** the
exception's type is *exactly* `java.lang.NullPointerException`, one `getMessage()` invocation is allowed.

Three gates, and together they remove each reason the discipline exists:

1. **Only when the field is null**, so an exception carrying its own message costs nothing. Every
   application exception takes the free path.
2. **Only for the exact type, not subclasses.** That makes `getMessage()` the JDK's own implementation,
   whose whole body is the native computation — no application code runs, and nothing acquires a Java-level
   monitor. The deadlock the rule guards against needs a lock to block on, and there is none. A subclass
   could override `getMessage()` with anything, which is why the check is exact rather than assignable.
3. **Bounded by the existing invocation budget, and an expiry is reported.** A `ReplyTimeout` renders as
   `<not read — getMessage() did not return within Nms; that thread is still executing it>` rather than as
   an absent key. Same reasoning as EVAL-5: a freeze must never be indistinguishable from "there was
   nothing there".

The invocation runs on both paths, including trace mode. A traced hit is armed `EventThread` and resumed
only after its snapshot is built, so a suspended thread is available either way, and the cost lands inside
TRACE-7's measured capture window — which is where ADR-0010 says a caller should be able to see it.

An absent message is an **absent key**, never an empty string. "No message", "no such field" and "the read
failed" are all reported the same way, because none of them is a message and a rendered empty one is a lie
a caller cannot see through.

## Rejected alternatives

**Field read only, as the issue specified.** Correct, free, and it delivers every exception except the one
in the issue's own evidence. Rejected because the issue's stated value — replacing a three-hour bisect,
where the *first* cause reported was wrong — comes entirely from the helpful NPE. Shipping the mechanism
without the case would have closed the issue and left the cost in place.

**Invoke `getMessage()` whenever `detailMessage` is null, for any type.** This is where the discipline earns
its keep: an application exception can override `getMessage()` to build a string from a lazily-loaded
association, and a debugger that runs that inside an event on a shared WildFly is the freeze this project
exists to avoid.

**Invoke the private native `getExtendedNPEMessage()` directly.** Narrower still — it cannot run any Java
at all — and rejected for pinning behaviour to a JDK-internal name. `getMessage()` on that exact type is
public API and stays correct if the internals move.

**Reimplement JEP 358 from the bytecode.** The throw location and a bytecode index are in hand and
`classfile.rs` already parses class files, so deriving the sentence ourselves is genuinely possible with no
invocation whatsoever. Rejected on size: it is a reimplementation of HotSpot's `ByteCodeHelper`, which is a
larger and more fragile thing than the issue it serves. Worth revisiting only if the carve-out above proves
unsafe in practice.

## Consequences

The discipline is no longer "never invoke inside an event" but **"never invoke application code inside an
event"** — a rule about whose code runs, not about the mechanism. A future describer that wants an
invocation has to make the same three arguments: no user code, no locks, bounded and reported.

`debug.set_exception_stop`'s and `debug.get_last_event`'s descriptions both state where the message comes
from, because a caller weighing a trace on a shared instance is entitled to know that one JDWP invocation
per messageless NPE is in the price.

Below JDK 15 the helpful message does not exist, and a snapshot correctly reports none. That is a real
behavioural difference across CI's three legs rather than a flake, and
`an_exception_snapshot_carries_the_jvms_own_message` gates on `Jdk::feature_version` and says so — the
version-locked-test trap from [#36](https://github.com/YgorPerez/java-debugging-mcp/issues/36), avoided on
purpose rather than discovered later.
