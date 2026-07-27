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

A fifth property followed from the fourth once the duration was visible: **ask for less**. `monitors_only`
(#17 item 3) skips the frame read and its per-frame lookups, which is where a dump's cost lives — measured at
245 packets / 33ms held against 770 / 117ms on a 60-thread dump. Bounding a freeze and shortening it are
different levers, and for the question this tool exists for — which threads are blocked on what — the lock
graph is the answer and the stacks are context.

That saving was predicted to *widen* with real stack depth. Measured against a real WildFly it does not —
see the consequence below. The mode is still worth having; the reason is the lock graph, not the arithmetic.

That mode forced a distinction the reply could not previously make. Frames were `Result<frames, why>`, and
"not requested" is neither: as an error it reports a healthy VM as unreadable, as an empty list it reports
every thread as idle. Both are findings, and this is not one — so `DumpStack` carries three states, and the
header attributes the omission rather than leaving it to be interpreted. For the same reason
`monitors_only` with `monitors:false` is **refused** rather than silently corrected: it asks for neither
locks nor stacks, so every row would come back empty, which is exactly the output that reads as "nothing is
contended".

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
- **There is a third cause, and it now has its own voice** (DUMP-4,
  [#47](https://github.com/YgorPerez/java-debugging-mcp/issues/47)). A JDWP thread id is a weak reference,
  so a pool that retires workers races every dump: threads the JVM listed can be gone by the time they are
  asked about. Those rows were being counted into `… +N more thread(s) (raise limit, or narrow with
  name_filter)`, which on TEST-10's churning pool meant 41 missing rows blamed on a `limit` of 500 that
  never bound, with two remedies offered that are both no-ops — neither raising a limit nor narrowing a
  filter can bring back a dead thread. Counted apart and stated apart now, and the two counts still sum to
  the shortfall.
- **Point 1 reads both ways.** "A running thread is never rendered as `(no frames)`" is one instance of a
  wider rule: the dump must not answer with the *opposite* state. A `ZOMBIE` thread was rendered as
  `running — … pass suspend:true`, which is that same mistake inverted — finished and running are opposite
  answers, and the remedy was unfollowable because a finished thread can never be suspended. A row now says
  which of the two it is, and the header's "pass suspend:true and these become readable" count excludes the
  threads a suspension could not rescue.
- The default budget was **provisional**, chosen from loopback timings where a round trip is
  sub-millisecond, and the assumption was tracked on #13 and then on its successor
  [#24](https://github.com/YgorPerez/java-debugging-mcp/issues/24).
  **It is no longer provisional, and it did not change.** It has since been tested against a
  production-shaped pool — `PoolShapeProbe`: 300 threads, 60 frames deep — and against added network
  latency via `LatencyRelay`, neither of which needs the shared instance. The budget *did* bind under that
  shape (a whole-pool deep dump truncated at 40%), and the answer was to make the dump cheaper rather than
  the freeze longer: line tables are now cached per dump, the same dump costs 1,625 packets instead of
  21,364, and it completes within the existing 2000 ms. See
  [ADR-0011](0011-line-tables-are-cached-per-dump-not-per-connection.md), which also records why the
  rejection below now applies only to caching *across* dumps.
- **The budget has now been read against a real WildFly, and 2000 ms stands** (TEST-8/#24, 2026-07-27). A
  WildFly 21 running a real war, loaded to 267 threads with request workers a median 328 frames deep, cost
  **332 packets / 38–144 ms held** for a default dump and **2,173 packets / 273–573 ms** for the widest dump
  anyone would ask for — roughly 3.5× inside the budget at its worst. Packet counts are debuggee properties
  and do not move with the wire, so they are the durable figure: at 2 ms round trip the default dump still
  fits, past ~5 ms it truncates, and a full dump truncates from 1 ms upward. That is the shape a safety
  default should have — it binds exactly when a dump is genuinely expensive — and it is the argument for
  keeping the number rather than raising it. The reading was taken on a **local isolated instance**, so it
  measures a WildFly-shaped pool, **not** the question of how long it is acceptable to freeze a VM other
  people are using; that remains a policy call rather than a measurement.
- **`monitors_only`'s saving is real under load and inverts when idle.** Same instance: loaded, it cut the
  full dump from 467 ms to 198 ms and the default from 144 ms to 35 ms (1.6–2.4×). **Idle, it was
  *slower*** — 114 ms against the full dump's 87 ms, and 545 ms against 394 ms — despite using ~40% fewer
  packets, because monitor reads are per-thread JVM work rather than cheap round trips, and with no deep
  stacks to skip there is nothing to save. So the prediction that the saving would widen with stack depth
  was wrong in both directions: at WildFly depth it is **narrower** than the 3× measured on probes, and
  without load it is negative. Reach for the mode because the lock graph is what you want, not because it
  is always cheaper.
