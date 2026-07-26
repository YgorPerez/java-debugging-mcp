# 0009 — `thread_dump` suspends only when asked, and verifies the resume

## Context

DUMP-1 asked for all-thread stacks and monitor ownership *without* suspending the VM as a side effect. Those
two requirements are in tension, because JDWP defines a thread's frames and locks as readable **only while it
is suspended**: a thread wedged on a monitor is *blocked*, not *suspended*, so its stack stays unreadable
until the debugger holds it too.

Taken literally, "do not suspend" produces a tool that answers "running — not readable" for every thread on
precisely the wedged instance it was built to diagnose.

## Decision

A caller's freeze is **explicit, bounded, measured, and verified** — four properties, none of which the
others imply:

1. **Explicit.** `suspend:true` is off by default. A dump that does not get it suspends nothing, reports what
   it could read, and says what would make the rest readable. A running thread is never rendered as
   `(no frames)`, because "unreadable" and "idle" are opposite answers on a wedged JVM.
2. **Never taken twice.** A VM that is *already* suspended is read as it is — neither resumed nor
   re-suspended.
3. **Bounded.** `max_suspend_ms` caps the collection, checked at a thread boundary so stopping never holds the
   VM longer to finish a row. On exhaustion the VM is resumed at once and the dump reports itself
   `INCOMPLETE` with the count it skipped. This is the ADR-0002 budget shape — counted server-side, charged
   per unit of work, stop announced rather than silent — applied to a suspension window instead of a hit
   count.
4. **Measured and verified.** The held duration is reported even on a fast dump, and the resume goes through
   `resume_all_fully`, reporting the ADR-0003 "the VM is STILL suspended" case rather than a clean-looking
   dump.

Properties 3 and 4 arrived later, via [#17](https://github.com/YgorPerez/java-debugging-mcp/issues/17): the
original decision made the suspension explicit, which was the important half, and left its *magnitude* open —
the window was bounded only by how long the collection happened to take, and the reply never said how long
that was. Recorded as part of the decision rather than as a footnote, because "explicit" alone turned out not
to be enough on a shared instance.

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
- Because the readable set depends on debugger suspension rather than application state, `only_suspended:true`
  is the way to get a dump with no unreadable entries in it on a running VM.
- **A truncated dump and a failed resume are reported separately.** Both are ways a dump can go wrong, and
  collapsing them would make "I stopped early" indistinguishable from "I could not un-freeze the VM" — the
  second is an emergency and the first is not.
- The default budget is **provisional**, chosen from loopback timings where a round trip is sub-millisecond.
  Calibrating it against a real thread pool is tracked as an assumption on
  [#13](https://github.com/YgorPerez/java-debugging-mcp/issues/13).
