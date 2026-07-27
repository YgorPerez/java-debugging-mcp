# 0011 — Line tables are cached per dump, not per connection

## Context

A `debug.thread_dump` renders each frame as `Class.method:line`, and the line comes from
`Method.LineTable` — one JDWP round trip per frame. Method *lists* were already cached on the connection
(`TypeCache`); line tables were not, so the identical question was asked once per frame per thread.

That was invisible at the scale it was tested. The dump's defaults were calibrated against
`ManyThreadsProbe`: 60 threads, 3 frames deep, on loopback. TEST-8
([#24](https://github.com/YgorPerez/java-debugging-mcp/issues/24)) argued the real 8180 is different in
three ways — hundreds of threads, stacks far deeper than 8 frames, and a network hop — and that only the
real instance could settle it.

Two of those three are properties of the **debuggee**, so `PoolShapeProbe` presents them exactly: 300
workers, 60 distinct frames each, parked. The third is latency, which `LatencyRelay` supplies in userspace.
Measured against that shape, on loopback:

| dump | packets | held |
| --- | --- | --- |
| whole pool, 60 frames deep | 21,364 | 4,686 ms |
| …at the default 2000 ms budget | 9,290 | **truncated at 40% of the pool** |
| `monitors_only` (no frames read) | 1,231 | 253 ms |

~19,000 of those 21,364 packets were line tables, covering roughly **60 distinct methods**. The threads of
a request pool are all standing in the same code, so nearly every one of those round trips asked a question
that had already been answered.

The relay then established what the wire contributes: over the same workload at 0/1/2/4 ms round trip, held
time rose by **~1.0 ms per ms of RTT per packet** (slope 0.997), against a raw loopback TCP round trip of
0.048 ms and a measured in-process cost of ~0.22 ms per packet. So `held ≈ packets × (ours + RTT)`, and on
an instance 1 ms away that dump would have held the VM for roughly **26 seconds**.

## Decision

Line tables are cached **for the duration of one dump call**, keyed by `(class, method)`, including
negative results — a native or abstract method answers `ABSENT_INFORMATION`, and a refusal has to be
remembered too or every thread re-asks and is refused again.

Effect on the same production shape: **1,625 packets / ~0.7 s**, and the whole-pool deep dump now completes
*inside* the default 2000 ms budget instead of truncating at 40%.

The scope is the decision. ADR-0009 records #17's rejection of caching line tables **across** dumps on
BP-4 grounds: `RedefineClasses` keeps the `referenceTypeID` and replaces the code, so a connection-lifetime
entry can serve a line number that is quietly wrong, and *a stale source line is worse than a round trip*.
Within a single call there is no such window — the VM is suspended for the read when `suspend:true`, the map
dies with the reply, and every cache hit is another thread standing in the code just read. This takes the
win the earlier decision declined without taking the risk it declined it for.

**The budget default stays at 2000 ms.** The remedy for a slow dump is fewer packets, not a longer freeze:
raising the budget would let a caller hold a shared VM longer, while the same dump now fits comfortably.
`max_suspend_ms` is a safety net, not a target.

**`limit: 40` and `max_frames: 8` also stay.** They were reviewed against a 306-thread pool with 60-frame
stacks: at the defaults such a dump costs 258 packets and holds the VM ~65 ms. Their binding constraint was
never round trips — it is how much output a reader can use, and 306 threads × 60 frames is ~18,000 lines of
stack. Cost stopped being the reason to keep them small; legibility remains one.

**A dump reports its own per-packet cost, and extrapolates a truncation.** The remaining half of #24 was
"read the real instance's thread count, stack depth and RTT, then do the arithmetic". A tool that can
measure those should not be asking a human to. So the cost line now carries the observed per-packet price
(`Cost: 258 JDWP packet(s), 3.13ms each`) — which is the RTT term, for the instance actually attached — and
a truncated dump says what finishing would have cost at the rate it was running (`at 18.6ms per thread, the
198 threads it skipped need ~3677ms more — about 5682ms for the whole set`). Both are extrapolations from
measurements already in hand, not predictions: the packet counter and the held clock were already there.

That reframes the defaults question. A default calibrated against one instance is a guess about every
other; a default plus a reply that states what *this* instance costs needs no calibration. Swept with the
relay, the **defaults hold the VM inside the 2000 ms budget up to roughly a 6 ms round trip** and truncate
past ~7 ms:

| nominal RTT | per-packet | held | outcome |
| --- | --- | --- | --- |
| 0 ms | 0.36 ms | 89 ms | complete |
| 2 ms | 3.08 ms | 779 ms | complete |
| 5 ms | 6.19 ms | 1,564 ms | complete |
| 8 ms | 9.15 ms | 2,039 ms | truncated at 34/306 |
| 12 ms | 13.19 ms | 2,017 ms | truncated at 20/306 |

So 2000 ms is right for a LAN-local instance, and on a slower link even a defaults dump truncates — which
is the safety net behaving correctly, and now says what it would have taken to finish.

**The win does not depend on the pool being uniform**, which is the obvious objection to measuring it
against 300 threads in identical code. Cost is `threads × fixed + distinct (class, method) pairs`, so
diversity is paid for **per distinct frame, not per thread**. Measured across the whole bracket:

| pool | distinct pairs | packets |
| --- | --- | --- |
| `PoolShapeProbe` — 300 threads, identical 60 frames | 60 | 1,625 |
| `MixedPoolProbe` — 300 threads, 10 handlers over a shared 40-frame prefix | 240 | **1,812** |
| no frame shared by any two threads (= the pre-cache measurement) | ~18,000 | 21,364 |

+187 packets for +180 distinct pairs — one each, the model exactly. A real request stack is mostly shared
framework (filter chain, security, dispatch, connection pool) with a handler at the bottom, which is the
middle row, so this holds on an app server for the same reason it holds here.

## Rejected alternatives

**Caching line tables on the connection**, which is where the reuse would be largest — a hot traced stop
point re-reads the same caller frames on every hit. Rejected for the reason ADR-0009 already gives: a
redefined class keeps its type id, and a wrong line number is worse than a slow one. Per-call scope gets
the bulk of the win (in a pool dump the reuse is *across threads*, which is intra-call) at no staleness
risk. Reconsider only with explicit invalidation on `ClassPrepare`/redefinition.

**Raising `max_suspend_ms` so a deep dump completes.** It would have "fixed" the truncation by holding a
shared VM for five seconds, and for twenty-six on a remote instance. The truncation was the safety net
working; the packet count was the bug.

**Treating `monitors_only` as the answer.** It was ~18× cheaper than a deep dump before this change, which
made it look like the fix. But it answers a *different question* — locks, not stacks — and recommending it
to callers who need stacks is telling them to ask for less because we were expensive. After the cache it is
only ~1.3× cheaper in packets, which is the honest position: it is the right mode when you want the lock
graph, not a workaround.

**Predicting a dump's cost *before* running it**, and narrowing or refusing it automatically. Attractive on
a shared instance, and rejected because the prediction is a range rather than a number: with line tables
cached, cost depends on how many *distinct* methods the pool's stacks cover, which is unknowable in advance
— anywhere between `threads × fixed + frames` and `threads × (fixed + frames)`, an order of magnitude apart
on a uniform pool. Reporting what a dump did cost, and what finishing it would have cost, is measurement;
reporting what one *would* cost is a guess wearing a bound's clothing. `max_suspend_ms` already caps the
exposure, and it truncates loudly.

**Caching the resolved line per frame** rather than the table. Two frames of the same method at different
bytecode indexes resolve to different lines, so this produces a plausible dump in which every frame of a
method shares one line number. Guarded by
`one_cached_line_table_resolves_each_bytecode_index_to_its_own_line`.

## Consequences

- A dump's cost is now bounded at roughly **5 packets per thread** plus one line table per distinct method,
  rather than one per frame per thread. `a_production_shaped_dump_costs_a_bounded_number_of_packets_per_thread`
  asserts ≤20 per thread — a packet count, deliberately, because it is deterministic and load-independent
  where a duration is neither. Verified to fail at ~70 with the cache defeated.
- `get_stack` and the trace caller-chain still fetch per frame. Neither has intra-call reuse worth caching
  (one stack, distinct methods), so the win does not generalise — and for traces the reuse is *across*
  calls, which is the rejected alternative above.
- The shared-instance defaults are no longer calibrated only on loopback: thread count and stack depth are
  reproduced by `PoolShapeProbe`, and latency by `LatencyRelay`. What still needs the real 8180 is its own
  parameters — how many threads, how deep, and the RTT to it — which one defaults dump and one ping answer.
- Per-packet cost is no longer uniform across workloads. With the cheap repetitive packets gone, what
  remains is larger replies (a 60-frame `Frames` reply), so the measured cost rose from ~0.22 ms to ~0.42 ms
  per packet even as the total fell 6.8×. `held ≈ packets × (ours + RTT)` holds as a model, but `ours`
  depends on reply size.
