# ADR-0036 — An invoking `trace_expr` is refused on the half of a monitor pair that does not own the lock

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** DUMP-8 ([#123](https://github.com/YgorPerez/java-debugging-mcp/issues/123))
**Extends:** [ADR-0035](0035-a-monitor-duration-is-the-debuggers-measurement-and-says-so.md) (which refused
`condition` on this kind for the same underlying reason, and left `trace_expr` on a caution)

## Context

`debug.set_monitor_stop`'s `trace_expr` is evaluated against the hit frame. For the **opening** half of a
pair that frame belongs to a thread which does **not** own the monitor named in its own snapshot:

- `MONITOR_CONTENDED_ENTER` (`blocked`) fires as the thread queues at a `monitorenter`;
- `MONITOR_WAIT` (`wait`) fires as it enters `Object.wait()`, which *releases* the lock.

An expression that invokes anything needing that monitor therefore cannot complete, and **JDWP has no way to
cancel an invocation**. `send_invoke`'s own doc comment has always said so: the budget
(`DEFAULT_INVOKE_TIMEOUT_MS`, 2000 ms) returns control to *us*; the debuggee thread stays where it is.

ADR-0035 refused `condition` on this kind for exactly this reason. `trace_expr` was left with a sentence in
the tool description — "field reads are safe, a getter that touches shared state under the same lock is not"
— on the judgement that the caller asked for the invocation and could weigh it.

## The measurement that decided this

Reproduced against `examples/probes/WedgeProbe.java`, which exists because `MonitorProbe` could not answer
the question: it has no method that acquires a contended lock and returns a value, and its longest hold is
400 ms — *inside* the budget, so an expression there merely waits and then succeeds, which would have looked
like evidence of safety. `WedgeProbe` holds for 3000 ms and carries a `synchronized int stamp()` on the lock
object itself.

On Temurin 11.0.32 and 21.0.12, arming `kinds:["blocked"]` with `trace_expr:"LOCK.stamp()"`:

```text
| LOCK.stamp() => <error: invocation did not return within 2000ms …>
```

and then, polling `debug.list_threads {only_suspended: true}` every 400 ms:

```text
0/7   0/7   0/7   1/7 0x2 wedge-contender [monitor]   1/7   1/7   …  (for the rest of the run)
```

**The three findings, in the order they change the decision.**

1. **The thread ends up suspended, permanently.** Not "inside a stalled call for a while" — suspended, by a
   stop point whose single promise is that it suspends nothing. The debuggee's own counter (`acquisitions`,
   printed on the probe's tick line) froze for the remainder of every run while the holder kept cycling, so
   the application looks alive with one thread dead. Nothing rescues it: the watchdog resumes a suspended
   **VM**, and the VM is running.

2. **The extra suspend arrives ~1.2 s AFTER the capture path has resumed the thread and moved on** — exactly
   the hold that was left when the budget expired. The JVM re-suspends the thread when the outstanding
   invocation finally completes.

3. **The debugger's own invocation re-enters the armed stop point.** The invoked method blocks on the
   monitor, generating another `MONITOR_CONTENDED_ENTER`, so the stop point reports contention *the debugger
   created*, at a location inside the invoked method, and spends `trace_max_hits` on it.

## Decision

**Refuse a `trace_expr` that calls a method when an opening kind (`blocked` or `wait`) is armed**, at arm
time, before anything reaches the debuggee. Field reads are accepted everywhere. On the closing halves
(`acquired`, `waited`) an invoking expression is accepted and works, because the thread owns the monitor by
then and a call needing it re-enters.

Refused rather than warned about, which is a change from ADR-0035's disposition, because the caller cannot
reliably tell: a getter that reads a field under `synchronized` looks exactly like one that does not, and the
price of being wrong is a wedged application thread on a JVM other people are using.

`JdwpError::InvokeTimeout`'s message now also says what the *debuggee* does — the thread is still inside the
call, the JVM re-suspends it when the call returns, and `debug.continue` or `debug.resume_thread` is the only
remedy. That covers every other route to a timeout, none of which this ADR changes.

## The rejected alternative, and why it is worth recording

**Fix the timeout path to release the thread** — verify the resume by reading the suspend count back and
resuming until it clears, which is ADR-0003's rule and what every other resume in this server already does.
This was the first choice, and it was implemented: `resume_thread_fully` in `jdwp-client`, called from
`try_record_trace` in place of the bare `let _ = resume_thread(thread)`.

**It does nothing at all.** Polled every 400 ms, with and without it, the sequence above is byte-identical.
Finding 2 is why: the extra suspend is applied when the invocation completes, long after the capture path has
finished. There is nothing left at hit time to verify.

It was caught only because **the negative test passed without the fix** — the test checked the thread listing
immediately, before the re-suspend landed, and reported success. That is ADR-0034's rule earning its keep (a
negative assertion must be seen failing before it is trusted) and it is the other half of the lesson TEST-38
recorded one release earlier: an assertion can fire correctly and still prove the wrong thing.

**A reaper in the watchdog** — record the thread on `InvokeTimeout` and have the existing watchdog clear any
such thread it finds suspended — is the only shape that would actually un-wedge it, since the suspend arrives
asynchronously. Rejected as out of proportion: it touches the watchdog, which is safety-critical, it would
leave the thread wedged for up to `JDWP_WATCHDOG_SECS` (default 120), and #123 explicitly scoped the general
uncancellable-invocation hazard out. If it is ever wanted, this ADR is the evidence it would start from.

## Consequences

- A caller who wants a getter's value on a contended lock must arm `acquired` instead, which the refusal
  names. That is a real loss of capability on `blocked`, and it is the price of the guarantee.
- `debug.set_monitor_stop` now has **six** up-front refusals, more than any other stop point. Five of them
  exist because JDWP does not mean what an argument reads like on this event kind; this one exists because
  the event kind's own timing makes an ordinary capability unsafe.
- The tool description and the argument text describe a refusal rather than advice, so the downstream
  toolkit's skills paraphrase a rule instead of a warning (`docs/toolkit-contract.md`).
- `WedgeProbe` and its two tests are checked in, following `probe_monitor_events.rs`'s precedent: the
  measurement is reproducible rather than quoted.
