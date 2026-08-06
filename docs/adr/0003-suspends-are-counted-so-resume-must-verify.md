# 0003 — Suspends are counted, so a resume must verify that the VM actually runs

## Context

JDWP counts suspends per thread: a thread suspended *n* times must be resumed *n* times. Nothing in this
codebase knew that. `suspend_all`/`resume_all` were plain VM-wide Suspend/Resume, `handle_pause` never
checked whether the VM was already stopped, and `ThreadReference.SuspendCount` was declared in
`commands.rs` and never implemented.

Measured against a real JVM before the fix — two `debug.pause` calls, then one `debug.continue`:

| | probe ticks after |
|---|---|
| one `continue` (depth 2 → 1) | **0** — still frozen |
| a second `continue` (1 → 0) | **+14** — running |

The consequence was the worst outcome in this project's threat model. Pausing at a breakpoint, or twice,
built a depth one resume couldn't clear; the watchdog then issued its single `resume_all`, reported
*"auto-resumed the VM"*, cleared `suspended_since`, and **never retried**. A shared JVM frozen permanently
and reported as rescued.

## Decision

1. `suspend_count` is implemented (`ThreadReference.SuspendCount`, set 11 cmd 12).
2. `resume_all_fully(probe_thread, max)` resumes until the probe thread's count reaches 0, returning
   `(resumes issued, remaining)` — bounded, because a thread also suspended individually by an
   `EventThread`-policy event may legitimately need more than one.
3. `debug.pause` is **idempotent**: an already-suspended VM is left alone, so a depth cannot accumulate
   through this tool's own front door.
4. `continue`, `panic` and the watchdog clear the whole depth and **verify before claiming success**. On
   failure the watchdog deliberately does *not* call `mark_resumed`, so it retries rather than going quiet
   on a false success.

## Rejected alternative

Tracking our own suspend depth in the session and resuming that many times. It drifts from reality the
moment anything suspends the VM that this session didn't issue — another debugger, an IDE left attached, an
`EventThread` event — and the whole point is to be right about whether the application is running. Asking
the JVM is the only answer that stays true.

## Consequences

- `debug.pause` while suspended at a breakpoint no longer overwrites the `StopPoint` cause with
  `ManualPause`, which had silently lost the ADR-0004 disarm target.
- The watchdog can now report *"tried to resume … but the VM is STILL suspended"*, which is a real state and
  strictly better than a false success.
- This is asserted by the **resume-honesty invariant matrix**, not by a happy-path test. The matrix used to
  be described in `TODO.md`; it is inlined below because that file is gone and this is the decision it
  belonged to.

## The resume-honesty invariant

*Read this before touching a resume path.*

Five reviews in, **every round's most serious bug was in the previous round's safety work**, and the watchdog
was wrong three times (SAFE-2 → SAFE-5 → SAFE-7). The shape never varied: a resume path was tested in the one
state its author had in mind and broke in a state nobody enumerated.

So there is a test for the invariant itself rather than another happy path
(`mcp_integration.rs`, `*_is_honest_from_every_suspended_state`):

> After **any** resume path, from **any** suspended state, the VM is genuinely running — or the reply said out
> loud that it isn't.

It is a matrix of **8 suspended states × 5 resume paths** (`continue`, `panic`, watchdog, `disconnect`,
`resume_thread`), asserted against the **probe's own output**, because every tool reports success either way —
which is exactly how these bugs survived. Each of SAFE-1, SAFE-4 and SAFE-7 was reverted in turn to confirm the
matrix names the offending `(state, path)` pair rather than passing anyway, and SAFE-11 was verified the same
way twice: dropping its `SuspendCount` check named `(ThreadSuspendTwice, OneThread)`, and leaving a pending step
armed on the thread it releases named `(Step, OneThread)`.

FILT-7 ([#91](https://github.com/YgorPerez/java-debugging-mcp/issues/91)) added `ConditionEscalated` — the
debugger, not the JVM, issuing the VM-wide suspend when a condition holds (ADR-0020). It reaches suspend depth
2 on the hit thread the way `BreakpointThenPause` does, but with no `debug.pause` anywhere in the sequence.
SAFE-11 ([#90](https://github.com/YgorPerez/java-debugging-mcp/issues/90)) added `ThreadSuspend`,
`ThreadSuspendTwice` and the fifth path. The two arrived in parallel branches, each extending a five-state
matrix, and **both states are here because the merge kept both rather than letting one branch's count win** — a
matrix that silently covers less than it claims is the exact failure it exists to catch.

The per-thread states are run against every path **including the ones that cannot fix them**: `debug.continue`
is about the VM's suspend depth and deliberately does not release a held thread, so its honest answer there is
to name what it left behind.

**If you add a resume path, add it to `Resume`. If you find a new way to leave the VM suspended, add it to
`Freeze`.** That is cheaper than the next review finding it, and it is the whole point of the matrix.

#91 also produced a state the matrix deliberately does **not** cover, because it is not a suspended one: a
condition that matched while the escalation *failed*, leaving one thread held and the application running. It
has its own test (`a_matched_condition_that_cannot_freeze_the_vm_reports_both_facts`), asserting the same
invariant in the same shape — whatever the reply says about the VM, the probe agrees — across both worlds a
failed suspend can leave behind.

**Its scope is resume honesty, not disarm honesty**, and the tests say so. A VM that resumes but is immediately
re-frozen by a still-armed stop point is the SAFE-2/SAFE-5 harm, and that half is covered by two tests that
measure the probe's tick **rate** after a rescue. Folding them together needs a repeating-breakpoint state whose
expectation differs per path, since `continue` may legitimately re-freeze and a rescue path may not.
