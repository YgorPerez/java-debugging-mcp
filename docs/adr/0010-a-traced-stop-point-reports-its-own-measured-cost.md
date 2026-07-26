# 0010 — A traced stop point reports its own measured cost, and the timer wraps the capture only

## Context

TRACE-6 ([#22](https://github.com/YgorPerez/java-debugging-mcp/issues/22)) established that trace mode is
*unfrozen*, not *undegraded*: capture is serialised through one JDWP connection and one event pump, so a
traced stop point tops out at roughly **720 hits/s** at the default `trace_frames: 3` (~1160 at 0), costing
~0.86 ms per hit plus ~0.53 ms for the caller chain. Those numbers went into the tool descriptions, the
argument docs and the skill's Rule 0.

They are true, and they are one measurement: one machine, one endpoint, one moment. A caller on different
hardware, or on a hotter site, has an estimate. What they need to decide whether to leave a trace armed on a
shared instance is what **their** stop point is costing **now** — and the debugger is the only thing that can
answer, since it already counts hits for `trace_max_hits` and merely lacked a clock.

The precedent is #17, from the same batch: `thread_dump` used to report only its packet count, leaving the
freeze to be inferred from a packet count and a guess at latency. It now reports the duration it held the VM.
TRACE-7 ([#26](https://github.com/YgorPerez/java-debugging-mcp/issues/26)) is that move for traces.

## Decision

`debug.list_stop_points` reports, per **traced** stop point, what it has actually cost. Three figures,
because none of them answers the question alone:

1. **Mean capture per hit** — the observed version of the documented ~0.86 ms.
2. **The rate it could sustain** (1/mean) — the observed counterpart of the documented ~720 hits/s ceiling.
   This is *capture-only* by construction: idle time between hits cannot flatter it.
3. **The rate hits are arriving at**, with the **share of the observation window spent capturing**. That
   share is the answer to "is this hurting the instance?", which neither of the first two gives: a cheap
   capture on a hot line and a costly capture on a quiet line are different problems with the same mean.

Four properties fix how it is measured:

- **The timer wraps the capture and nothing else** — the snapshot and the caller-chain read. Not the
  condition evaluation that may drop the hit, not the resume, not the budget arithmetic, not this
  bookkeeping. Charging our own work to "what a traced hit costs" would report the debugger's overhead as the
  debuggee's price. Same reason #17 timed the dump's suspend/resume pair rather than the whole call.
- **Only recorded captures count.** A hit dropped by a `condition` or by the method filter captured nothing,
  so it cost nothing — the same rule that keeps `trace_max_hits` meaning "exactly N snapshots".
- **The arrival rate spans first capture to last, over N−1 intervals.** One capture prices a hit but
  establishes no interval, and is reported as exactly that rather than as a rate over a zero-width window.
- **A re-arm resets the observation.** The figures describe the current arming, like the budget
  (ADR-0004 keeps the *definition* across a disarm; the measurement is not part of the definition).

**A traced stop point with no captures reports `UNMEASURED`, not `0.00ms`.** A rounded-down zero reads as
free, and unmeasured is not free — the same rule that makes an unrequested thread dump stack a third state
rather than an empty one (ADR-0009), and that makes a trace self-disarm announce itself rather than going
quiet.

**A suspending stop point reports no capture cost at all.** It performs no capture. Its price is the freeze,
which the watchdog and `thread_dump` report. An absence here is correct; a zero would be a claim.

## Rejected alternatives

**Leaving the documented figures as the answer.** They are an estimate presented as fact, and the gap only
shows up where it matters most — unusual hardware, a hot site, a deep stack. #22 itself deferred this as
"probably a follow-up", which it was.

**Timing the whole event-pump iteration.** Simpler, and one line instead of a struct — but it folds in the
condition evaluation, the resume round trip and our own bookkeeping. The number would then rise when the
*debugger* got slower and be reported as the cost of tracing.

**Acting on the number** — throttling, auto-disarming above a rate threshold, refusing a trace on a hot site.
Deliberately out of scope: `trace_max_hits` already bounds the exposure (ADR-0002), and a tool that
silently declines to trace what it was asked to trace is worse than one that says what it costs. Report
first.

**Carrying the cost across a re-arm.** The disabled gap sits inside the observation window while producing no
hits, so a logpoint re-armed minutes after a self-disarm would report an arrival rate far below the site's.
Diluted-but-continuous loses to accurate-per-arming.

**A separate `debug.trace_cost` tool.** One more call to know whether the last call was safe. The listing is
where a caller already goes to see what is armed, and the cost belongs beside the budget and the caller
depth it explains.

## Consequences

- Each of the four stop-point kinds carries a `TraceCost`, updated in the pump beside the budget charge.
  There is deliberately **no index keyed by JDWP request id**: like `decrement_trace_budget`, the recorder
  scans the four maps, because a parallel index is a second source of truth that can outlive its entry
  (ADR-0005).
- The figures are per **arming**, not per session lifetime. A caller comparing two periods must read the
  listing at both, or clear and re-arm.
- The documented ~720 hits/s now has an independent check. Against `CallerProbe`, whose traced line is
  reached three times per ~150 ms iteration, the debugger measured **1.65 ms mean / ~608 hits/s / 20.5 hits/s
  arriving / 3.4% of the window** — consistent with #22's ~1.39 ms and ~720/s on faster hardware. The
  integration test asserts the reported arrival rate lands on the probe's *known* rate, so a plausible
  constant cannot satisfy it.
