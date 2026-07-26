# 0008 — Caller frames are read by fetching the whole stack and truncating

## Context

TRACE-5 records `trace_frames` callers above a traced hit. The obvious implementation asks the debuggee for
exactly the frames it needs: `Frames(thread, 0, 1 + trace_frames)`.

`ThreadReference.Frames` answers `INVALID_LENGTH` when `length` exceeds the frames a thread actually has,
and a thread is routinely shallower than the requested depth — `main` sits two frames under a helper. The
read is the same call that fetches the hit frame, so the failure lost **the whole snapshot, locals
included**, on exactly the shallow stacks a small depth was meant to cover. Nothing errored; the trace line
still looked like a valid hit with no locals in scope.

Measured against `CallerProbe` with `trace_frames: 3` before the fix — one armed request, three call paths:

| path | frames on the stack | recorded |
|---|---|---|
| `main → alpha → record` | 3 | **nothing** — no callers, no locals |
| `main → beta → record` | 3 | **nothing** — no callers, no locals |
| `main → beta → nested → record` | 4 | callers and locals, complete |

## Decision

Request `-1` (all frames) and truncate to `1 + trace_frames` on our side. Depth `0` keeps the original
single-frame request, so turning the feature off costs exactly what it did before — every live thread has at
least one frame, so length `1` is always valid.

`get_stack` already did this, for this reason; its call site carries the comment `-1 means all frames to
avoid INVALID_LENGTH errors`. The trace path simply failed to learn from it.

## Rejected alternatives

**Requesting the exact count.** The efficient and obvious choice: one reply sized to what is needed, rather
than a whole stack to use four frames of it. It cannot work — the length is a hard error, not a clamp — and
its failure mode is silent data loss on the common case rather than a visible error.

**Calling `FrameCount` first and requesting `min(want, count)`.** Correct, but trades a silent failure for an
extra round trip on **every hit**, on the hot path this feature runs on. Fetching frames we discard costs one
packet at ~28 bytes per frame; a second request costs a full round trip.

## Consequences

- A deep `WildFly` stack returns frames that are immediately dropped. The per-frame lookups (signature,
  method list, line table) are still paid only for the frames kept, which is where the real cost is — so the
  waste is reply bytes, not round trips.
- **A test must request a depth no path can satisfy, or it proves nothing.** A depth every path can satisfy
  passes against the broken implementation; that is how this shipped green the first time.
  `traced_hits_record_which_caller_reached_them` asks for 3 callers where two of its three paths have 2, and
  asserts the locals survive alongside the chain.
