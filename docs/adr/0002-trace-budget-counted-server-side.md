# 0002 — The trace-hit budget is counted server-side, not with JDWP's `Count` modifier

## Context

`trace:true` makes a stop point non-suspending: it snapshots the hit and resumes the thread. TRACE-2 made
that available on watchpoints and exception breakpoints, which made it easy to arm something that fires
thousands of times a second. `MAX_TRACES` bounds *memory*, not per-hit work in the target — each hit costs
a `get_frames`, a variable table, a `get_frame_values` and the describers. So trace mode, advertised as the
safe option, could degrade the app worse than a suspending breakpoint, which at least stops at the first hit.

## Decision

Each traced stop point carries `trace_budget` (from `trace_max_hits`, default 200, `0` = unbounded).
`charge_trace_budget` decrements it per **recorded** hit and disarms the stop point at zero, leaving a note
that `get_traces` reports.

## Rejected alternative

JDWP's `Count` modifier (modKind 1), which the original TODO item proposed and which looks strictly better
because it expires *inside* the JVM — no packet is sent once it lapses.

It cannot express what is needed. `Count` reports **only the Nth occurrence**, not the first N: with
`count: 5` you get hit #5 and nothing else. The requirement is "record the first N hits, then stop", so
`Count` would silently record one trace instead of N.

Recorded here because it was nearly re-proposed a batch later by the same agent that had rejected it — the
JVM-side expiry is genuinely attractive and the reason it doesn't work is a detail of the spec that is easy
to forget.

## Consequences

- Only a **recorded** hit is charged, so the "exactly N traces then it stops" contract holds even when a
  condition skips some.
- `get_traces` must announce the self-disarm, or the silence afterwards reads as "no more hits".
- `list_breakpoints` shows the remaining budget.
- `Count` is still the right tool for `hit_count` ("stop on the Nth hit"), which is exactly what it means,
  and that is where `set_breakpoint_ex` uses it.
