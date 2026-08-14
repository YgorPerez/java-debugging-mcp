# 0051 — A rescue's report is qualified where it is read, and never deleted

## Context

A **rescue** leaves one record on the session, and two tools render it: `debug.get_last_event` beside the
event it belongs to, and `debug.list_stop_points` at the top of the listing, where SAFE-2 put it because a
caller who walked away needs it before anything else.

Nothing clears that record. Not a drain, not a resume, not a fresh suspension. SAFE-10 (#69) had already
fixed the same shape in `get_last_event` by stamping the record with the event sequence it was written at
and rendering it only against events no newer than that stamp; the listing was left reading it raw.

So one rescue at minute five captioned every later listing for the life of the session — including after
the caller had done the one thing the record tells them to do. SAFE-14 (#198) is that report:

```
⏰ watchdog auto-resumed the VM after 300s and disabled bp_1 … — re-arm it when ready
📍 1 breakpoint(s) …
   ✓ [bp_1] com.example.OrderRepo:88     Hits: 3
```

Each half is correct and the pair is false, which is #69's harm one tool along: a caller who has to
reconcile them spends a detour re-verifying a hit that was fine.

**The obvious fix is the one that does not work, and this ADR exists mostly to say so.** #198 proposed
scoping the listing with SAFE-10's watermark and called it the cheapest option. It was implemented as a
throwaway and **measured against a live JVM: the banner printed in exactly the same place**. Re-arming a
stop point pushes no event, so the newest sequence is still the one the record was stamped at, the
watermark still matches, and nothing changes. The watermark answers *is a newer event on screen* — the
right question where the record is rendered beside an event, and the wrong one where it is rendered beside
the stop points themselves.

## Decision

**A rescue's record carries the stop points it disarmed, and the listing qualifies it against what those
ids read as now.** Where every one of them is still disarmed the reply is byte-for-byte what it was, which
is SAFE-2's case entire. Where one has been re-armed, or cleared away, a line under the banner says so:

```
⏰ watchdog auto-resumed the VM after 300s and disabled breakpoint bp_1 at … — re-arm it … when ready
   ↳ bp_1 has since been re-armed, so that instruction is already carried out — the entries below are the
     current state of the stop points.
```

**The record is never rewritten and never deleted.** It is the only surface a **failed** rescue has
(`⚠️ watchdog tried to resume the VM … but …`), and a report that is deleted because one of its claims
expired takes a still-frozen debuggee with it.

**So each line discharges the one claim it is about.** `CONTEXT.md`'s **Rescue** entry is where the
vocabulary lives: a rescue is reported in two claims and only one of them can expire. Re-arming settles
the disarm half of a failed rescue's note and says nothing about the VM, so the wording is about the
instruction rather than about the report — *everything above is now history* would tell a caller their VM
is running while the same note says it is not.

**This is ADR-0004 one level up.** That decision is that an automatic disarm *disables* a stop point rather
than deleting it, because deleting destroys what the caller typed. This is the same rule applied to the
**report** of that disarm: keep it, qualify it, never destroy it — and for the same reason, that the thing
destroyed is unrecoverable and the caller is the one who paid for it.

Ids travel with the record rather than being recovered from its prose, because `bp_1` is a prefix of
`bp_11` and a listing that qualified the wrong stop point would be the same class of wrong this removes.

## Rejected alternatives

**Scope the listing by SAFE-10's watermark.** #198's own first choice, and **rejected on a measurement
rather than an argument**: it leaves the banner exactly where it was, because a re-arm pushes no event.
It fails in the other direction too — once any later event arrives it hides the banner from a caller who
walked away, which is the case SAFE-2 exists for. Recorded here at length because it is what the next
reader will reach for: the watermark is the established mechanism for this shape of bug, and it is sound
where it is used.

**Clear the record when the suspension it describes is superseded.** Keeps SAFE-2's case and drops a
failed rescue's warning while it is still true — the VM is still frozen and the only thing that said so is
gone. A carve-out for the failed arm was considered and rejected as a second rule that has to be got right
in a path nobody exercises.

**Qualify without naming anything** — reword the banner as history unconditionally. One line, no state, and
it still prints *re-arm it* over a stop point that is already armed, so it addresses the tone and not the
contradiction.

## Consequences

- `debug.list_stop_points` and `debug.get_last_event` now scope the same record by different tests, on
  purpose. The listing reads the stop points; the event tool reads the watermark. Neither is a fallback
  for the other, and a future reader who unifies them re-introduces this bug on one side or the other.
- **SPENT counts as re-armed.** Every path that sets `spent` clears `enabled`, and a stop point the
  watchdog disarmed has no request left to fire, so only a re-arm could have spent it. Reading `enabled`
  alone put `re-arm it` over an entry reading SPENT — found in review, with nothing in the suite covering
  it, and now asserted.
- A rescue that disarmed nothing — a manual pause (ADR-0021's arm has no stop point), or one whose stop
  point had already been cleared — names nothing and is never qualified. Its report is only that the VM
  was released, which never stops being true.
- The reply text is pinned in `mcp-server/tests/reply-fragments.txt`, including the **empty** case: the
  contract is as much that a current report renders unchanged as that a stale one is qualified.
