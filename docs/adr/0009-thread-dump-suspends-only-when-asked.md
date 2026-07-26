# 0009 — `thread_dump` suspends only when asked, and verifies the resume

## Context

DUMP-1 asked for all-thread stacks and monitor ownership *without* suspending the VM as a side effect. Those
two requirements are in tension, because JDWP defines a thread's frames and locks as readable **only while it
is suspended**: a thread wedged on a monitor is *blocked*, not *suspended*, so its stack stays unreadable
until the debugger holds it too.

Taken literally, "do not suspend" produces a tool that answers "running — not readable" for every thread on
precisely the wedged instance it was built to diagnose.

## Decision

`suspend:true` is an explicit request, **off by default**. It suspends, collects, resumes via
`resume_all_fully`, and **verifies** — reporting the ADR-0003 "the VM is STILL suspended" case rather than a
clean-looking dump. A VM that is *already* suspended is read as it is and left that way. A default dump
suspends nothing, reports what it could read, and says what would make the rest readable; a running thread is
never rendered as `(no frames)`, because "unreadable" and "idle" are opposite answers on a wedged JVM.

The same principle inverts a default elsewhere: for **method-exit requests**, `trace` defaults to `true` and a
broad suspending request is refused outright. Where the unsafe mode can freeze a shared instance fastest, the
safe mode is the default and the dangerous one is opt-in.

## Rejected alternatives

**Never suspending** — the literal reading of the issue. Every thread reports as unreadable on the one JVM
state the tool exists for, which is a correct-looking tool that answers nothing.

**Suspending automatically whenever a dump is requested**, so the output always looks complete. This is the
SAFE-4 mistake: the tool would pause a shared instance to make its own reply nicer, and the caller would not
know it had. Making the suspension *explicit* was the point of the requirement; making it *impossible* was
not.

**Resuming an already-suspended VM after reading it.** It would discard the breakpoint state the caller is
standing in. Re-suspending it instead would build a suspend depth that one resume cannot undo (ADR-0003).
Reading it as-is and leaving it is the only option that neither destroys nor accumulates state.

## Consequences

- This **deviates from #15's literal wording**, reading "does not suspend the VM as a side effect" as "does
  not suspend *silently*". Recorded here because issue and code otherwise appear to disagree, and the
  interpretation is the load-bearing part of the design.
- Making the suspension explicit left its *magnitude* open: the window was bounded only by how long the
  collection took, and the reply never said how long that was. Closed by
  [#17](https://github.com/YgorPerez/java-debugging-mcp/issues/17) items 1–2, which report the held duration
  and bound it with `max_suspend_ms` — the ADR-0002 budget shape applied to a suspension window instead of a
  hit count. The early exit resumes and verifies like every other path here, so this decision still holds.
- Because the readable set depends on debugger suspension rather than application state, `only_suspended:true`
  is the way to get a dump with no unreadable entries in it on a running VM.
