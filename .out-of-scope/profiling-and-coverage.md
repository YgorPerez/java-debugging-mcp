# Profiling, Memory Timelines and Code Coverage

This debugger does not profile the debuggee, does not sample its allocations, and does not report which
of its lines were executed. There is no partial implementation waiting to be finished, and no JDWP
command set left unwired: **the protocol has none.** PROF-1 (#140) proposed adding them and was closed
as out of scope; this file is why, so the next person asking gets an answer instead of re-deriving it.

## Why this is out of scope

**It is the opposite posture to everything else here.** Every tool in this server answers a question by
*reading* a JVM it assumes it must not disturb, and reports what the reading cost — ADR-0023 has a heap
query that ships with the pause it imposed printed in its own reply. Profiling is a whole-application
measurement regime: you turn it on, it changes the timing of the thing you are measuring, and it stays
on. On the shared 8180 instance this project is built around not freezing, that is not a diagnostic.

**It would need a second protocol, against a promise `README.md` makes.** JDWP has no sampling, no
allocation-profiling and no coverage command set — this is not a gap in the implementation, it is a gap
in the wire protocol. Reaching them means JFR, JVMTI or a bytecode-instrumenting agent, and the last two
mean **modifying the running application to measure it**. `README.md` says no agent is required, and
that promise is a large part of why this attaches to a production-shaped app server at all.

**The tools that do this are better at it, and people already have them.** JFR ships with the JDK. APM
products exist. A developer who wants a flame graph is not blocked by this server, and would not choose
an LLM debugger to produce one.

**The nearest legitimate need is already served.** People reach for a profiler to ask *why is this slow*.
`debug.set_monitor_stop` answers the contention half — which lock, held how long, measured by the
debugger and said so (ADR-0035) — and `debug.thread_dump` answers where threads are sitting. Neither
suspends by default. PROF-1's own body could not name a question those two cannot answer, and that is
the test it failed.

## What would change this

Not "somebody wants it". Concretely:

1. **A question it answers that `set_monitor_stop` and `thread_dump` cannot**, stated as a question. The
   issue asked for this of itself and did not produce one.
2. **A decision that a second protocol is acceptable**, taken deliberately rather than as a consequence.
   Everything here assumes one port and one connection; JFR or JVMTI is a second channel with its own
   lifecycle, failure modes and safety story.
3. **`README.md`'s no-agent promise amended or kept**, explicitly. Coverage-via-instrumentation cannot
   keep it.
4. **Split first.** Profiling-via-JFR and coverage-via-instrumentation share only the row in
   `docs/comparison.md` they arrived on. They are different features with different objections, and
   arguing them together is how both stay unresolved.

## Related

- `docs/comparison.md` — where the row came from, and since DOC-16 (#161) also the place that sorts
  `kpanuragh/xdebug-mcp`'s seven profiling and coverage tools against this file, so the question is
  answered without opening it
- `.out-of-scope/method-entry-events.md` — the precedent: a high-volume observation mechanism rejected
  in favour of a cheaper one answering the same question
- ADR-0023 — a heap query ships, and reports the pause it imposed
- ADR-0035 — a monitor duration is the debugger's own measurement, and says so
