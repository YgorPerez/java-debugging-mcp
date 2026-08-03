# ADR-0031 — A trace whose suspend policy has been taken over is reported, not refused, and the listing stops calling it a trace

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** TRACE-12 ([#117](https://github.com/YgorPerez/java-debugging-mcp/issues/117))

## Context

A `trace:true` stop point's entire promise is that it snapshots and resumes and **freezes nothing**. It is
the only mode this project considers safe on the shared 8180, and the `jdwp-trace` skill is written around
it.

That promise is not a property of the stop point. It is a property of the **event set** — one composite per
hit, carrying one event per request that matched at that location, under a single suspend policy which is
the *strongest* any member asked for. Measured while settling BP-6 (#102), on Temurin 17.0.20 / 21.0.12 /
25.0.3, identically: three `BREAKPOINT` requests at one bytecode location, two armed `EventThread` and one
armed `All`, arrive as one composite with `suspend_policy = All`.

So arming a suspending stop point on a line that is already traced converts every trace there into a
VM-freezing one, on every hit — possibly hours later, possibly from another session against the same JVM.
The reverse order is equally bad and likelier: somebody with a suspending breakpoint on a line adds a
`trace_expr` expecting the cheap thing and gets the expensive one.

**Nothing told the caller.** Both replies said what they had always said, and `debug.list_stop_points` went
on printing an unqualified `(trace)` next to a stop point that was freezing the whole VM. BP-6's fix
(`9a211c7`) had already made a mixed set *behave* correctly — the traced members still get their snapshots,
and the set is handed to the suspending path for the resume decision — so what was left was purely what the
caller is told. ADR-0020's amendment recorded the mechanism and explicitly left this half open.

## Decision

**Allow the arm and warn. Do not refuse.** The maintainer's call, and the reasoning is the asymmetry the
issue identified: this is not a lie about the *new* stop point, it is a change to an *old* one, and
refusing would mean a caller cannot suspend on a line they are already tracing — which is a legitimate
thing to want, and often exactly the next step after a trace shows you something.

That distinguishes it from the FILT-9 precedent (ADR-0027), where `instance_id` on a static method is
refused. There the reply about the thing being armed would itself have been false. Here the new stop point
does precisely what it says; the casualty is a different stop point's earlier promise, and the fix is to
withdraw that promise out loud rather than to block the arm.

Three places say it, because each is reached by a different reader:

1. **The arm reply, in both directions.** Arming a trace onto an already-suspending line says
   `THIS TRACE WILL FREEZE THE VM ANYWAY` and names what is responsible. Arming a suspending stop point
   onto a traced line says `THIS ALSO MAKES N TRACED STOP POINT(S) FREEZE THE VM`, names every one of them,
   and says that clearing this stop point restores them. Both name the mechanism, because it is not
   guessable from anything else on the surface.
2. **`debug.list_stop_points`**, where somebody asking "why did the VM freeze?" actually looks. An
   escalated stop point is marked `(trace — SUSPEND POLICY OVERRIDDEN)` instead of `(trace)`, with a line
   naming what escalated it. An unqualified `(trace)` on a stop point that is freezing the VM is the single
   most misleading thing this listing could print.
3. **The batch reply**, as a roll-call, since a wildcard can arm dozens and forty paragraphs is not a
   warning anybody reads — the same shape DISC-8's stale-bytecode roll-call takes.

**Only `All` counts as escalating.** A *conditional* stop point is armed `EventThread` and escalates on our
side once the condition holds (ADR-0020) — a decision taken after the composite has already been
delivered, so it does not freeze the other members at hit time and must not be reported as if it did.
Likewise two traced stop points on one line are left alone: `EventThread` plus `EventThread` is still
`EventThread`, and warning there would train the reader to skip the warning.

**Overlap is decided by armed bytecode location, not by source line.** Two stop points can share an event
set exactly when they share a location, and a stop point may hold several — BP-4's inlined `finally`
copies and BP-5's per-classloader copies. Comparing the caller's *line* would miss a stop point armed via a
method entry that resolved to the same index, and would also wrongly pair two classloaders' copies of one
line, which are separate types and separate sets.

Disabled and spent stop points are excluded: with no live JDWP request they are in nobody's set.

## Alternatives considered

### Refuse the second arm (the issue's other option)

Rejected, per the decision above: it removes a legitimate capability to prevent a warning. It would also
make the *order of two independent calls* decide whether the second is possible, which is a worse surprise
than the one being fixed.

### Change the traced stop point's policy instead, so the set stays `EventThread`

Not available. The set takes the strongest policy any member asked for, so the only way to keep it
`EventThread` is for no member to ask for `All` — i.e. to refuse the suspending arm, which is the
alternative above wearing a different hat. Silently downgrading the *suspending* stop point would break its
promise instead, and it is the one the caller just asked for.

### Report it only in the listing

Cheaper, and rejected: the caller who arms the suspending stop point is the one who caused the change and
the one positioned to undo it. Making them go and look would be the same "silence reads as an answer"
failure in a smaller font.

### Diff what each call armed, rather than sweeping the session

The batch path sweeps every armed stop point instead of working out which of its own members landed on
somebody else's line. The overlap is a property of the *location*: it can be created from either direction
and by a call that armed something else entirely, so a sweep is both simpler and correct in cases a diff
would miss.

## Consequences

- `CONTEXT.md` § **Trace** no longer says the listing goes on printing `(trace)`, and § **Event set** no
  longer describes the silence as current. ADR-0020's amendment, which left this open, now points here.
- ADR-0020's own heading said `0019` while its filename, the index and #117 all said 0020. Corrected in
  passing.
- `render_breakpoint_line` gained a branch and was split — `trace_marker`, `overridden_trace_note`,
  `render_classloader_and_rearm` — because doctor's complexity gate caught it at 17/15. The listing reads
  better for it.
- The check costs no JDWP packets: it is a walk of the session's own stop-point table.
- **Not covered:** a wildcard family's per-member arm reply. Families get the batch roll-call and every
  member is marked in the listing, but the individual `(pattern → N armed)` lines do not each carry the
  paragraph. A wildcard refuses `line` and arms at method entry, so colliding with a specific traced line
  is possible but rare, and the roll-call names it when it happens.
