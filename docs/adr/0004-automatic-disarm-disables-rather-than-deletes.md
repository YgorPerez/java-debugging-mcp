# 0004 — An automatic disarm disables a stop point rather than deleting it

## Context

Two mechanisms disarm a stop point without being asked: the watchdog, when a suspension outlives
`JDWP_WATCHDOG_SECS` (so the VM isn't re-frozen on the next hit), and the trace-hit budget of ADR-0002.

Both originally *removed* the entry. That destroyed whatever the user had typed by hand — the `condition`,
the `trace_expr`, the thread filter — which is precisely what the watchdog's own design note said not to do:
"prefer surgical: clearing every stop point on a timeout would silently throw away a careful setup." It then
threw away the offender's setup.

## Decision

`disarm_request` clears the JDWP request and marks the stop point **disabled** (`request_id: None`,
`enabled: false`), keeping the definition. One `debug.toggle_stop_point` re-arms it. This applies to all
three kinds — line breakpoints, exception requests and watchpoints — so exception requests and watchpoints
gained a disabled state and the re-arm information to go with it.

## Rejected alternative

Continuing to delete, and telling the caller to retype it. Rejected once BP-1 had shipped a disabled state
for line breakpoints: the machinery to do better already existed, and a debugger that discards a carefully
built conditional breakpoint on a timeout is hostile in exactly the situation where the user has stepped away.

## Consequences

- `list_stop_points` shows disabled entries with a `✗` and an explanation, so a disarmed stop point is
  visible rather than mysteriously absent.
- A re-armed stop point gets a **fresh trace budget** — it was disarmed *because* the old one ran out, so
  re-arming with zero left would fire once and immediately disable itself again.
- Two existing tests had asserted the *old* behaviour (that the stop point was gone). They now assert the
  stronger guarantee: kept, disabled, and re-armable.
- Re-arming needs the location, which is why ADR-0005 and the by-name re-resolve (BP-4) exist.
