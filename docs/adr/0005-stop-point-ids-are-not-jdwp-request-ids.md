# 0005 — Caller-facing stop-point ids are independent of JDWP request ids

## Context

Stop points were keyed by their JDWP request id: `bp_<request_id>`, `exc_<request_id>`,
`watch_<kind>_<request_id>`. That was fine while a stop point was armed exactly once and then cleared.

ADR-0004 made disable/re-arm a normal operation, and re-arming assigns a **new** JDWP request. So the id
changed underneath the caller: `toggle_breakpoint {bp_5, enabled:true}` returned a breakpoint now called
`bp_9`, and a caller repeating their original call got "not found". Any stored id died on the first
disable→enable round trip — in the tool whose whole purpose is to be scripted.

## Decision

Ids come from a per-session counter (`DebugSession::next_stop_id`), allocated once and kept for the stop
point's whole life. The JDWP request id is an internal detail: still *reported*, never the identity.

## Rejected alternative

Returning the new id and expecting callers to track the change. It makes every id a caller holds
provisional, which is unusable for scripting, and the reply saying so doesn't help a script that already
stored the old one.

## Consequences

- A breakpoint's id survives disable → re-arm, so trace records attributed to it stay comparable across
  the gap.
- The id is allocated in `handle_set_breakpoint` before we know whether it arms now or defers, so a
  deferred breakpoint keeps the same id when the class loads and it arms for real.
- Because the request id is no longer the identity, the *location* has to be stored to re-arm — and stored
  ids go stale across a redeploy, which is why re-arm re-resolves by name (BP-4).
