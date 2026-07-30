# 0019 — A conditional stop point decides on one thread, and escalates to a VM-wide suspend only when the condition holds

## Context

`suspend_policy_for(trace)` returned `SuspendPolicy::All` whenever `trace` was false, so a *conditional*
non-traced stop point was armed at `All` like any other. Every hit therefore stopped every thread,
`evaluate_condition_on_thread` ran with the whole application frozen — a `get_frames`, a variable table
read, a `get_frame_values`, plus any invoke in the condition — and `resume_all` let it go again when the
answer turned out to be *false*.

The cost was paid on **every hit regardless of the outcome**, which is precisely backwards: `condition` is
the argument you reach for to make a stop point cheap on a busy shared instance, and it was the most
expensive thing you could arm. On the infotravel 8180 the instrumentation points that matter are all too
hot for an unfiltered stop: the app-wide error sink has 2153 call sites, the DAO read chokepoint ~1900,
and `InfoTravelException` is constructed 3166 times. `condition: "reserva.id == 4711"` on any of them meant
freezing every thread thousands of times to learn "no" thousands of times.

`trace:true` + `condition` was already safe — `EventThread` policy, condition checked in
`try_record_trace`, and a condition-skipped hit is not charged to the trace budget — but it never
suspends. So "stop when this is true", the entire point of a conditional breakpoint, was unreachable at
acceptable cost on the only JVM anyone can attach to.

Measured on `CondProbe` (JDK 17, five runs per arm). An unrelated CPU-bound thread, across a window of
120 non-matching hits:

| conditional stop point armed at | units of work the other thread completed |
|---|---|
| `All` (before) | **10–14** |
| `EventThread` + escalation (after) | **81–119** |

The debugger's replies are byte-identical in both arms. That is why the test reads the debuggee's own
stdout and nothing else.

## Decision

A conditional line breakpoint is armed at `SuspendPolicy::EventThread`
(`suspend_policy_for_line(trace, conditional)`). The JVM holds only the hit thread; the condition is
evaluated on it; and `store_reportable_event` **escalates** — issues `VirtualMachine.Suspend` itself —
only on the hits where the condition holds. A false condition costs one thread briefly held and nothing
else. A true condition ends in the same observable state as before: VM suspended, event buffered, alert
pushed, caller expected to resume.

Four consequences had to be decided rather than assumed:

**1. The escalation window is real, and is stated rather than closed.** Between the condition returning
true and `VirtualMachine.Suspend` completing, every thread except the hit thread is still running. So the
state the caller goes on to read is the state a round trip *after* the hit, not the state at the instant of
it. That is a genuine semantic change and it is in the `debug.set_line_stop` description and the
`condition` argument's own documentation, because this repo's standard is that a tool says what it cannot
promise. A caller who needs the instant of the hit itself wants a stop point with **no** condition, which
the JVM freezes for us before it tells us anything.

**2. The hit thread is never released around the escalation.** It stays held by the event's own
`EventThread` suspension throughout, so the frame the condition just read is the frame `get_stack` finds.
The price is a suspend depth of 2 on that thread — its own hold plus the VM-wide one — which
`resume_all_fully` already handles (ADR-0003), and which the resume-honesty matrix now covers as its sixth
`Freeze` state, `ConditionEscalated`.

**3. A failed escalation reports both halves, and measures the second one.** "The condition matched" alone
reads as an ordinary suspending hit and sends the caller to `get_stack` on a moving target; "the suspend
failed" alone throws away the hit they were waiting for. So the event record carries a `FailedEscalation`,
`get_last_event` prints `[escalation] …` beside `[suspended]`, and the pushed notification carries the same
sentence.

Whether the application is still running is **verified against the debuggee** — another thread's suspend
count — rather than deduced from the error. Deducing it is the SAFE-7 assumption pointing the other way,
and it is wrong whenever the suspend lands and the answer does not come back. That was not theoretical: the
first version of the test used a fault relay that rewrites the *reply*, so the suspend landed and the VM
stopped while the reply said it had failed — and the assertion caught the message lying. Three outcomes are
therefore reported distinctly: verified running, verified stopped after all, and could-not-tell (which is
reported as running, because distrusting a good frame costs less than trusting a moving one).

**4. The watchdog and `hit_count` are unchanged, deliberately.**

*Watchdog.* Its clock starts at `mark_suspended`, which now happens at the escalation rather than at the
hit — so a non-matching hit no longer arms it at all, which is right: nothing VM-wide is being held. A hit
whose escalation *failed* still calls `mark_suspended`, even though the VM is running, because the hit
thread **is** held and this is the only record that anything holds it; without it one thread of a shared
JVM would stay suspended forever with nothing able to notice. The reply is where the distinction is drawn,
not the bookkeeping. One exposure narrows and none widens: a condition that hangs (a blocking invoke) used
to freeze the whole VM outside the watchdog's view and now holds one thread.

*`hit_count`.* It is a JDWP `Count` modifier and expires **inside the JVM**, counting *hits*, not
*matches*. So `hit_count: 5` with a condition means "check the condition on the 5th hit", not "the 5th hit
where the condition holds", and if the condition is false on that hit the stop point is spent. This was
true before FILT-7 and is unchanged by it — the policy change moves where the condition is evaluated, not
when the JVM decides a request has lapsed. Recorded here and in the `hit_count` documentation because the
two arguments read like they compose and do not.

## Rejected alternative

**Keep `All` and document the cost.** The change is genuinely not free: it introduces the escalation
window in decision 1, and "the state you read is not the state at the hit" is a harder thing to explain
than "conditions are expensive". Documenting the cost would have kept the semantics exactly as they were.

Rejected because the cost is not one a caller can act on. There is no cheaper way to arm the thing they
wanted — the whole point of the argument is to stop being noisy, and the alternative offered (`trace:true`)
answers a different question, since it never suspends. So the documentation would read "this feature is
unusable on the instance you have", which is a note, not a fix. Against that, the window is bounded by one
JDWP round trip and only opens on hits that matched; a caller who cannot tolerate it can arm the stop point
without a condition and get exactly the old behaviour, which the freeze-everything design gave nobody a way
to opt out of.

## Consequences

- `condition` is now the cheap argument it always read like, on the stop-point kind that accepts it.
  Extending it to the other three kinds is #83 and independent — that issue changes *where* a condition is
  accepted, this one changed *how* one is evaluated.
- `[suspended]` in `debug.get_last_event` is no longer a pure function of the event's suspend policy: a
  failed escalation prints `[suspended] false` beside an event whose policy says a thread was suspended.
- `trace:true` + `condition` is untouched: `try_record_trace` still owns that path, still evaluates on the
  event thread, and a condition-skipped hit is still not charged to the trace budget (ADR-0002).
- The test harness gained `FaultRelay::start_refusing`, which drops a command outbound and answers it from
  the relay. The difference from a `Fault` on the same command is the difference between a JVM that will
  not act and a JVM that acts and misreports, and both were needed to pin the two halves of decision 3.
