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
- This is asserted by the resume-honesty invariant matrix, not by a happy-path test — see `TODO.md`.
