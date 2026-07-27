# 0013 — A default dump selects by name family, not by creation order

## Context

`debug.thread_dump` took the first `limit` threads JDWP's `AllThreads` handed it. That is *creation*
order, and it is not an arbitrary slice of a JVM — it is a biased one. The JVM's own threads exist first,
the container's next, and the request workers a caller came to look at exist **last**, because an
application server does not start its request pool until everything it depends on is up.

Measured on a real WildFly 21 running a real war, loaded to 267 threads (TEST-8,
[#24](https://github.com/YgorPerez/java-debugging-mcp/issues/24)), a **default dump returned zero
application threads**. All 40 slots went to:

```
Reference Handler, Finalizer, Signal Dispatcher, Notification Thread, Common-Cleaner,
Reference Reaper, MSC service thread 1-1 … 1-8, ServerDeploymentRepository-temp-threads - 1,
DestroyJavaVM, ServerService Thread Pool -- 1, DeploymentScanner-threads - 1/2,
ServerService Thread Pool -- 38, management I/O-1/2, management Accept,
default I/O-1 … I/O-16, Timer-0
```

Meanwhile 13 `default task-*` threads sat a **median 328 frames deep in application code** and were never
read. The header said:

```
🧵 Thread dump — 40/267 thread(s)
```

which reads as a representative sample of a big pool. A caller looking at forty parked infrastructure
threads concludes the server is idle, at the exact moment thirteen request threads are wedged. That is
this repo's recurring failure mode — a check that reports success without having looked — in the tool
whose entire job is answering *"it's wedged, which threads are blocked on what?"*.

## Decision

**Read the cheap half of every thread's row, then spend `limit` on a round-robin across name families.**

1. **Triage every thread.** One pass reads each thread's **name and status** — two flat round trips with
   no per-frame lookups behind them, ~2 packets against the ~8 a full row costs. `name_filter` and
   `only_suspended` are applied here, so the limit is spent on threads that are actually readable.
2. **Group by name family.** A family is the thread's name with every run of digits collapsed to `#`:
   `default task-7` and `default task-91` are one family, `default I/O-3` is another. Crude on purpose —
   see the rejected alternative below.
3. **Round-robin.** Take one thread from each family, in first-appearance order, before taking a second
   from any. No single pool can spend every slot; 40 slots across ~25 families reaches every family,
   including the 13-thread one that mattered.
4. **Say so.** When anything was left out, the header states the rule, the arithmetic and how to read the
   rows, and the truncation footer names the biggest groups it withheld:

   ```
   🔀 Chose 40 of 267 by NAME FAMILY, not by the order the JVM listed them in: one thread from each of
      the 25 distinct names (digits ignored, so "task-3" and "task-91" are one family) before a second
      from any, so no single pool can spend every slot. … Rows below are printed in creation order.
   …
   … +227 more thread(s) (raise limit, or narrow with name_filter) — biggest groups not shown:
      37 × "default task-#", 26 × "default I/O-#"
   ```

   Silent when nothing was withheld, and silent on a single-family dump: round-robin over one family *is*
   creation order, and announcing a rule that did nothing is noise. That is what makes
   `name_filter: "default task"` behave exactly as it did.
5. **Select fairly, present stably.** Rows are printed in creation order regardless. The caller asked what
   the JVM is doing, not what order the debugger decided to ask in — and it means an *untruncated* dump is
   byte-for-byte what it was before this decision.

This is [ADR-0008](0008-caller-frames-fetch-the-whole-stack-and-truncate.md)'s shape — fetch wide, then
truncate deliberately — applied to threads instead of frames.

### The cost, which does grow

Reading every thread's name and status costs `2 × threads` packets whether or not they are used. On the
267-thread WildFly a default dump goes from **332 packets to ~790**, ~2.4×. That is affordable against
what this tool already does: ADR-0011 records the widest dump anyone would ask for at 2,173 packets /
273–573 ms held, roughly 3.5× inside the 2000 ms budget, so the new default lands near a third of an
already-accepted worst case.

It is paid **only when the dump is truncated.** When `limit` covers the whole JVM the triage pass reads
exactly the name and status the old single loop read, thread for thread — which is why
`a_production_shaped_dump_costs_a_bounded_number_of_packets_per_thread` (300 threads, ≤20 packets each) is
unmoved.

The triage pass is inside `max_suspend_ms` and gets **at most half the remaining window**. A dump that
spent its entire budget deciding what to read and then read nothing would be a worse answer than the bug
this fixes, and on a slow wire that is exactly what an unbounded first pass would do. Threads it never
reached are counted into the same `unread` the reply already reports.

## Rejected alternatives

**Raising the default `limit`.** The obvious fix and the wrong one, ruled out on the issue. On the measured
instance you would need `limit` in the high 40s just to see the *first* request thread, and that number is
a property of how many selectors WildFly happens to start. It costs more and still guarantees nothing.

**Walking the list backwards — newest first.** Cheap, needs no data at all, and reaches a pool that was
created last. It fails on the very fixture built for this: `ChurnProbe`'s stable workers are created after
the housekeeping threads but *before* an endless stream of churn workers, so newest-first returns 40
threads that will not exist in a second. A request pool is not the newest thing in a JVM, it is the newest
*long-lived* thing, and the list cannot tell those apart.

**A vocabulary of known framework thread names** — demote `default I/O-*`, `MSC service thread *`,
`Reference Handler`, and so on. It works on WildFly and nowhere else, it is guessing at somebody else's
naming, and it goes stale the first time they change it. Collapsing digits needs no such vocabulary:
numbering the workers is the one thing every pool in every framework actually does.

**Ranking by "interestingness" — stack depth, blocked state, application packages in the frames.** Ruled
out on the issue, and rightly: deciding needs a read per thread, which costs the thing the ordering is
trying to save. `threadStatus` is affordable and is read anyway, but it does not separate a working request
thread from a parked selector — both are `running` to JDWP.

**Reading only the name in the triage pass** and leaving the status to the row build. ~30% cheaper, and it
was the first version written. It puts the whole first pass between `AllThreads` and every status read,
and on a pool that turns over several times a second that is the difference between a thread that has
*died* and one that has died **and been collected**. TEST-10's churning-pool test caught it at once: across
three runs of twelve dumps it could no longer observe a single `[zombie]` row. Two packets per thread buys
data that is about the JVM the caller asked about.

## Consequences

- **`AllThreads` order is not creation order for long** — the discovery that reshaped the test fixture.
  HotSpot's live thread list is *compacted* as threads die, so a short-lived predecessor holds no position:
  `ChurnProbe`'s eight stable workers, started last behind 48 churn workers, were measured at ids
  `0x8..0xf` — **inside** the default limit — once the first churn generation had retired. Only a thread
  that is still alive when the dump runs can stand between a caller and what they came for, which is
  exactly why an app server reproduces this and a burst of work does not. The probe now starts 40 immortal
  `io-selector-*` threads first, and reproduces the WildFly reading permanently rather than for a third of
  a second.
- **A probe's worker lifetime is now coupled to how long a dump of it takes.** Adding those 40 threads
  lengthened a dump of `ChurnProbe` by ~60%, which pushed the churn population's reads past death *plus* a
  GC interval and made TEST-10's `[zombie]` state unreachable. `LIFE_MS` went 300 → 600 to put the deaths
  back inside the window. Recorded because it will bite again the next time the probe grows.
- **A dump of a small JVM is unchanged.** Nothing is withheld, so no ordering line is printed and the rows
  are in the order they always were.
- **`limit` is documented as a rule rather than a size**, and the reply repeats it. The acceptance
  criterion was that a caller must be able to know what the 40 they got are without reading the source;
  the header is where that is answered, because the header is what was misleading.
- **The withheld tally is per family, capped at five groups.** "227 more" answers *is this dump short?*;
  naming the groups answers *short of what?*, which is the question that decides between raising `limit`
  and reaching for `name_filter`.
