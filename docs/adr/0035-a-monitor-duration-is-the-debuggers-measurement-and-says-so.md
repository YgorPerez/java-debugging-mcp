# ADR-0035 — A monitor duration is the debugger's measurement, and every reply says so

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** DUMP-7 ([#96](https://github.com/YgorPerez/java-debugging-mcp/issues/96))
**Amends:** [ADR-0027](0027-an-instance-filter-is-offered-only-where-it-was-measured-to-apply.md) (adds a
fourth measured-inert case, and a second kind of one)

## Context

The four `MONITOR_*` event kinds (43-46) were named in the event-kind table and had **no decoder**: the event
reader fell through to `EventKind::Unknown`. So the only lock answer this tool could give was
`debug.thread_dump`, which cannot read a running thread's monitors and therefore needs `suspend:true`. That
made "requests are hanging on a lock" the one wedged-app-server question that *forced* a suspension of a
shared instance — the exact thing the tool's safety posture exists to avoid.

The brief asked a snapshot to carry, "for the `WAITED` and `ENTERED` variants — the elapsed time or timeout
flag those events carry". **No monitor event carries an elapsed time.** The wire layouts are:

| event | payload beyond requestID |
|---|---|
| `MONITOR_CONTENDED_ENTER` (43) | thread, object, location |
| `MONITOR_CONTENDED_ENTERED` (44) | thread, object, location |
| `MONITOR_WAIT` (45) | thread, object, location, **timeout** (long) |
| `MONITOR_WAITED` (46) | thread, object, location, **timed_out** (boolean) |

`ENTERED` carries no timing at all, and `WAIT`'s `timeout` is what the caller passed to `wait(…)` rather than
how long it waited — a `wait(5000)` that returns in 3 ms still reports 5000. "How long was it blocked", the
question the brief's own framing names, is not on the wire.

That mattered more than a missing field, because there was an *available* wrong answer: printing `timeout`
would have looked like a duration and been plausible on every reply.

## Decision

### The events are paired, and the reported duration says who measured it

`ENTER` is timestamped and matched to the following `ENTERED` on the same (thread, monitor); `WAIT` to
`WAITED` likewise. The resulting figure is a **debugger** measurement, so every surface that prints one says
so: `blocked_for=4200ms (measured by the DEBUGGER across both events — no monitor event carries a duration,
so this includes our own capture latency)`.

It has to say so for two reasons that a bare number hides. It includes this server's own capture cost
(~0.86 ms per hit before caller frames, TRACE-7) plus event-pump queueing, which is noise against the
multi-second block a wedged server is asked about and a material fraction of a 5 ms one. And it requires
**both halves armed** — one half can only report that the event happened.

The two pairs are named apart (`blocked_for`, `waited_for`) rather than sharing one `elapsed`. Blocking is
involuntary and a long one is a fault; `Object.wait()` is voluntary and a long one is often a healthy idle
worker. One label would flatten the distinction the reply exists to draw.

### The pair is keyed on (thread, monitor, **which pair**)

`Object.wait()` releases its monitor and re-acquires it on wake, and that re-acquisition can itself be
contended — so one thread can legitimately have a `Blocked`→`Acquired` and a `Wait`→`Waited` measurement
outstanding on the *same* object at the same instant. Keyed on (thread, monitor) alone they overwrite each
other and report one duration as the other.

### A duration threshold filters what you READ, and drops what it cannot judge

`min_duration_ms` is a server-side filter and the reply says so plainly: JDWP has no duration modifier, so the
packet has already arrived and has already cost the debuggee its notification. It shrinks the trace buffer and
nothing else. The bounds that act *inside* the JVM stay the `ThreadOnly` filter and the trace-hit budget.

Two consequences of it are stated at arming time rather than left to be discovered:

- **The opening half stops recording and becomes pure timestamping.** At that instant nothing has elapsed to
  compare, so recording it would fill the buffer with the noise the threshold was set to remove — and spend
  the budget doing it. At the default 200 a contended lock would exhaust its budget on "started blocking"
  lines before one long block was reported.
- **A pair whose duration could not be measured is dropped.** This started out the other way round, on the
  reasoning that "the lock was acquired, duration unavailable" beats a silence. That is right with no
  threshold and wrong with one: a caller who asked for blocks over 200 ms has said what they want, and an
  **unmeasured** pair may have lasted 1 ms. (That word is exact: no matching start was seen. A pair whose
  duration is *unmeasurable* is the other case — only one half armed, or a suspending stop point — and its
  reply names the fix. `CONTEXT.md` keeps the two apart, because this line used to name the wrong one.)

`Hits` is counted **before** the threshold, which is what keeps the resulting silence readable: `Hits: 900`
beside no snapshots means "contended constantly, never for that long", and `Hits: 0` means nothing contended
it. Those are opposite findings and they read identically if only recorded snapshots are counted.

### `ClassOnly` means two different things on these four kinds, so it is refused where it misleads

The JDWP spec defines modKind 4 per event kind, and the monitor reading applies **only to the wait pair**.
Measured on Temurin 11.0.32 over 3-second windows against `MonitorProbe`:

| arming | events |
|---|---|
| all four, unfiltered | 434 |
| `blocked` + `ClassOnly`(**location** class `MonitorProbe`) | **45** |
| `blocked` + `ClassOnly`(**monitor** class `FastLock`) | **0** |
| `wait` + `ClassOnly`(**location** class `MonitorProbe`) | **0** |
| `wait` + `ClassOnly`(**monitor** class `TimeoutLock`) | **74** |

So `monitor_class` is accepted with `wait`/`waited` and **refused** with `blocked`/`acquired`, where the JVM
would apply it to the class of the *code that blocked* instead. Passing it through would arm a stop point
scoped to a code location under a reply claiming it was scoped to a lock type — "only `Hashtable` locks"
against "only blocking inside `Hashtable`'s methods". That is ADR-0027's rule reaching a new case: not a
modifier that is accepted and ignored, but one that is accepted and **applied to something else**.

### `instance_id` is refused, measured inert

`InstanceOnly` tests the frame's `this`, and the monitor is a different object from whatever the blocking code
is executing on. Measured with a real object id read off the probe's own static field, against a probe whose
every frame is **static** — so `this` is null and nothing could legitimately match: the request armed cleanly
and reported all three of its locks. The fourth entry in ADR-0027's table.

It is *declared* in the schema rather than omitted, because an undeclared argument is silently ignored, which
would leave a caller with a reply claiming the stop point was scoped to one lock while it reported every lock
in the JVM.

### There is no `condition` on this kind, and that is a safety decision

A condition is evaluated on the hit thread, and a thread suspended at a `monitorenter` is blocked on the very
lock in the snapshot. An expression that invokes anything needing that monitor cannot complete, so the
debugger would wedge the thread it is reporting on. `min_duration_ms` is this kind's filter and it needs
nothing from the debuggee. The same caution is stated for `trace_expr`, which a caller does ask for explicitly.

### `trace:false` requires a `thread_id`

Every other stop point has something to narrow a suspending arming to — a line, a class, a method, a field —
because the caller *chose* where it fires. Contention is not chosen: it is wherever threads collide, including
inside the JDK, so a VM-wide freeze lands on the next acquisition of any hot lock and can re-fire the instant
it is resumed. One named thread is the only narrowing that exists, so it is required rather than advised.

### Capability bit 18 is decoded although no command here issues its request

Bit 17 (`canRequestMonitorEvents`) is consulted at arming, and the refusal names the fallback rather than only
the fact — `debug.thread_dump` with `suspend:true` is what is left on such a JVM. Both bits read `true` on
Temurin 11.0.32 and 21.0.12.

Bit 18 (`canGetMonitorFrameInfo`) is a deliberate exception to `VmCapabilitiesNew`'s standing rule that a bit
arrives with the check that consults it. What consults it is the arming reply: a snapshot names the lock, the
thread and the blocking location, and the obvious next question — *where in this thread's stack was the lock
taken* — is answerable on a JVM with the bit and not on one without. Saying which of the two a caller is on
costs one already-issued command; leaving it out invites the reading that the tool never reports frame depth.
Nothing here claims the frame-depth query exists, which is the line `IDSizes` crossed (CLEAN-1, #27).

## Consequences

**Five stop-point kinds now, and the shared machinery took it unchanged.** One JDWP request per armed kind
under its own `mon_<kind>_…` id — the shape `debug.set_field_stop` already uses for `modify` + `access` — so
the hit tally, the trace budget, the cost accounting, `clear`, `toggle` and `panic` all needed a branch each
and no new mechanism.

**Arming is all-or-nothing across a pair.** If the second kind fails to arm, the first is cleared again. A
half-armed pair is not a degraded success: it is a stop point that reports events and can never measure a
duration, under an id whose reply said it would. This differs from the batched *pattern* arming elsewhere,
where each row is an independent question about a different class and a partial answer is the honest one.

**Clearing one half degrades the other, so clearing says so.** The survivor keeps reporting events and loses
its measurement; if it carries a `min_duration_ms` it can no longer record anything at all. The listing
re-derives the pairing from the session rather than trusting the record's own flag, because clearing the
partner is exactly what makes that flag wrong.

**`panic` clears the pairing state as well as the requests**, and so does a re-arm. Left behind, a stale start
would be handed to whatever is armed on that pair next and reported as a duration reaching back before that
stop point existed — a number wrong by minutes rather than by milliseconds.

**Two stop-point tallies were under-reporting before this, and now are not.** `debug.list_sessions` and
`disconnect`'s "cleared N stop point(s)" summed five of the six kinds and had omitted `method_exits` since
METH-1: a session holding nothing but method-exit requests reported `0 stop point(s)` while
`list_stop_points` listed them. The number a caller checks to see whether they left anything armed could not
see the kind most able to freeze a shared JVM. Fixed for both kinds rather than widened to two.

## What JDK 11 caught, and the lesson

The unmeasurable-pair decision above was found by running the suite on JDK 11, where it failed
**deterministically** while passing on 17, 21 and 25. The cause is not a JDK difference at all: the first
closing events after arming routinely have no matching start, because those threads were *already* blocked
when the request went in. Whether the first pair through is the fast lock or the slow one is timing, and the
slower JVM lost it every time.

So the handoff's rule earned its keep again — **run all three JDKs on every feature, not just at the end** —
and with the corollary that a race a faster JVM hides is not a race that is less likely, it is one whose
outcome has changed.

Running the suite also surfaced a **pre-existing** instance of the same shape: FILT-6's
`conditional_field_and_method_exit_traces_filter_without_charging_the_budget` used `Probe::launch` where its
first act is arming a watchpoint, which cannot be deferred. It failed on JDK 11 with "Class 'CondKindsProbe' is
not loaded yet", verified failing at `adb5345` without any of this change. Fixed here with `launch_running`,
which is what TEST-17 (#49) documents that call for.

The probe itself taught the third one. `MonitorProbe`'s first version produced **one contended entry in
thirteen seconds**: `synchronized` is unfair on HotSpot, so a holder looping straight back into `monitorenter`
barges the thread already queued on it, systematically. It printed ticks and its counters moved, so it looked
right from the outside. A 20 ms gap outside the block fixed it and took it to ~15 pairs/s.

## Alternatives considered

**Printing `MONITOR_WAIT`'s `timeout` as the duration.** The available wrong answer, and the reason this ADR
leads with the wire layout. It is the argument, not a measurement, and it would have been plausible on every
reply — the worst shape of defect this codebase has.

**Landing the decoder without the arming.** The wire layer was written first and reverted, because
`events.rs` argues against exactly that: *a decoded variant nothing can arm only implies a capability that
isn't there* (the `MethodExit` doc's note on `MethodEntry`).

**Computing the pairing inside `capture_trace`.** Where the rest of a snapshot's detail is built, and
impossible: it receives a connection and a stop point, never a session, and the pairing state is per-session
by nature. The duration is computed in `record_one_traced_event` and *injected* into the record the capture
produced, which is why it is the one detail no single event could supply.

**Measuring the duration on the suspending path too.** Refused, and the reply says why rather than leaving a
gap: suspending at the opening half stops the thread from ever reaching the closing one until the caller
resumes, so the elapsed would be mostly the caller's reading time. A number that measures the debugger instead
of the debuggee is worse than no number.

**A duration threshold as a JDWP modifier.** There is none. Stating that plainly is the whole point of how
`min_duration_ms` is described.

**Offering a `location_class` filter for the contended pair**, since `ClassOnly` does work there — on the
blocking code's class. Rejected as a second argument for one modifier: `ClassOnly` takes one exact type rather
than a pattern, so it would mean "only blocking inside this one class", which is narrow enough to be rarely
what anyone wants and easy to confuse with `monitor_class` at the call site.

**`ObjectReference.MonitorInfo` (9/5), "who holds this object's lock" on demand.** Left where the brief put
it: a different question from the event stream, and its own issue if it is wanted.
