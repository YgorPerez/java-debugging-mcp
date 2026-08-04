# ADR-0038 — Independent reads share one round trip, and independence is a licence granted per call site

**Status:** Accepted
**Date:** 2026-08-04
**Issue:** PERF-1 ([#100](https://github.com/YgorPerez/java-debugging-mcp/issues/100))

## Context

#100 was filed as `L` on the reading that *"`send_command` takes `&mut self` and awaits its reply before the
next command can be sent"*, and that pipelining is therefore *"available in principle and unused"*.

**The first half of that sentence is true of one wrapper and false of the machinery under it.** Measured by
reading the code rather than the brief:

- `EventLoopHandle::send_command` already takes `&self` (`jdwp-client/src/eventloop.rs`).
- `event_loop_task` already holds `pending_replies: HashMap<u32, PendingReply>` **keyed by packet id**, and
  `handle_outgoing_command` writes a command, inserts into that map, and **returns without awaiting the
  reply**. The `select!` arms for outgoing commands and incoming packets are independent of each other.
- `route_reply` already delivers each reply to its own entry by id.
- Packet ids already come from an `AtomicU32` on a `&self` method, and `TypeCache` is already behind
  `Mutex`es.

So N commands could always be outstanding at once, already correlated by id. The serialisation lived in
exactly two places above the transport: `JdwpConnection::send_command(&mut self, …)`, whose entire body is
`self.event_loop.send_command(packet).await`, and the call sites holding `&mut session.connection`.

**This moves the risk, and it moves it in the direction that matters.** #100's first named risk is that
framing is unrecoverable — true, and unchanged, and *not on this path*: framing lives in
`spawn_packet_reader`'s own task, which is the whole of ADR-0018. Issuing more commands does not go near it.
The brief cites ADR-0018 as evidence that pipelining is *possible*; it is also the thing that makes it
*safe*. #100's second named risk — that ordering is load-bearing in places — is untouched by any of the
above and is the entire remaining problem.

The JVM's side of the contract is not this codebase's to decide, so it was read rather than assumed. The
JDWP specification:

> The JDWP is asynchronous; multiple command packets may be sent before the first reply packet is received.

> The id field is used to uniquely identify each packet command/reply pair. A reply packet has the same id as
> the command packet to which it replies. This allows asynchronous commands and replies to be matched. […]
> The id field must be unique among all outstanding commands sent from one source.

Two guarantees and one silence. Commands may overlap; a reply names its command. **Nothing says replies come
back in the order the commands went out** — which is why `CONTEXT.md` rejects the word *pipelined* for this,
since HTTP's sense of it promises exactly the ordering JDWP declines to.

## Decision

### The primitive is a split, not a mechanism

`EventLoopHandle::send_command` is separated into `issue` — hand the command to the loop, return an
`InFlight` — and `InFlight::reply`. `send_command` becomes the composition of the two and is unchanged for
every existing caller. Nothing in the loop changed at all.

`JdwpConnection::read_independently(&self, Vec<CommandPacket>) -> Vec<JdwpResult<ReplyPacket>>` issues a wave
and returns one positional result per command. It takes `&self`, which is what the transport always needed;
the `&mut self` on `send_command` guarded nothing the loop required.

**Issue order is write order, and neither is completion order.** The loop dequeues FIFO and writes in that
order, so a command issued first reaches the JVM first. That is worth stating precisely because it is the
weakest of the three properties a caller might assume, and the one most likely to be mistaken for ordering.

### Every reply is awaited, including after one has failed

There is no first-error-wins arm. `try_join!` semantics would abandon the *wait*, not the work: the commands
are already on the wire, JDWP has no way to recall one, and the JVM answers them regardless. Abandoning would
save nothing measurable in packets or in JVM time, while leaving replies arriving for nobody. A caller that
wants to stop at the first failure can do that to the returned `Vec` at no cost to the wire.

A failed command therefore cannot damage its siblings, and — separately — cannot damage the stream:
dropping an `InFlight` is safe because the reader task consumes whole packets irrespective of who is
listening. That safety is ADR-0018's, not this ADR's.

### The window is a safety bound before it is a tuning knob

`INDEPENDENT_READ_WINDOW = 16` caps how many commands may be unanswered at once, and a caller passing a
thousand packets is bounded by the window rather than by their list.

The cycle it rules out is a genuine deadlock, and every arrow in it is real: the event loop blocks writing a
command because the JVM stopped reading; the JVM stopped reading because it is blocked writing replies; it is
blocked writing because our receive buffer is full and the reader task is parked on a full
`PACKET_CHANNEL_DEPTH` channel; and the reader is parked because the loop — blocked in that write — is not
draining it. What breaks it is that the loop can only block once the *send* buffer fills, and a JDWP command
is 11–43 bytes: sixteen is under a kilobyte against a send buffer of at least sixteen. A window of a
thousand, which expanding a thousand-element collection would otherwise ask for, is a different
conversation.

Sixteen is also the ceiling on buffered reply memory — a reply may be `MAX_PACKET_SIZE`, so sixteen
`AllClasses`-shaped replies would be 160MB — and it caps the win at sixteen-fold, since `n` reads cost
`ceil(n / 16)` round trips. That trade is taken deliberately on the safe side: sixteen-fold is most of the
available fan-out on the reads #100 names.

### What it buys is round trips, and the glossary's `Packet` entry now has to say so

The primitive sends the same commands, each still taking one id from the same counter, so `packets_sent()` and
every packet-bound test in `mcp_integration.rs` are unaffected **by construction** — asserted anyway, in
`a_wave_costs_exactly_one_packet_per_read`, because the accounting is easy to break invisibly. What changes is
that `n` reads cost about one round trip instead of `n`, and that a suspension under which those reads happen
is shorter by the difference. On loopback the difference is nearly nothing; this exists for a remote JVM.

**A conversion may still lower the packet count, and one did.** Deduplication is a separate saving that a wave
makes obvious rather than causes: gathering a stack's reads into one list is what exposed that the same
`(class, method)` was being asked about sixty-six times, so the stack-walk conversion below removes 119
commands as well as 199 round trips. Nothing here ever *raises* the count — that is the invariant, and
speculation is the only thing that could break it.

`CONTEXT.md`'s `Packet` entry justified packets-as-cost-unit by **equating them with round trips**. That
equation is severed the moment a call site is converted, so the entry was left alone by the commit that added
the primitive and sharpened by the one that first made the two numbers diverge — a glossary should describe
the tree as it is, and while everything was still serialised the entry was accurate. It now says the packet
figure is an upper bound on what a caller waits for rather than a proxy for it, and the cost lines still
report packets: deterministic, load-independent, and comparable between releases, which is why they were not
changed to report round trips instead.

### Independence is established per call site, and nothing here can check it

`read_independently` names a licence — `CONTEXT.md`'s **independent reads** — and its doc comment is not a
grant of it. Three sequences in this server do not have it, each for its own reason: a suspend must land
before a frame is read at all; a frame's variable *names* must be known before its values mean anything; and
a watchpoint's **old value** is only readable while the pending store has not yet committed, so that read
cannot be moved out of its window.

The primitive therefore landed with its correlation tests and **no call site converted at all**, which is
#100's own instruction (*"convert one call site, with a measured before/after"* and *"do not convert
everything in one change"* are the same instruction) and was independently reviewable. One call site is
licensed below.

Every licensed read is named `read_*_independently`, so `grep -r _independently` lists the whole licensed
surface. That is deliberate: a licence nobody can enumerate is one that spreads.

## The first call site, and what measuring it actually found

`project_query_rows` — the row projection behind `debug.run_named_query` (EVAL-11, ADR-0037) — reads each
row's runtime type and then each row's fields. **Two waves, and the boundary between them is the licence
being refused**: a row's field ids come from its type, so its values read cannot be issued until its type
read has answered. The two waves could be collapsed into one, which would look like a further optimisation
and would be wrong.

`a_heterogeneous_result_reads_each_rows_fields_off_its_own_type` asserts the refusal rather than trusting it,
against a probe query whose rows alternate two types that share no field. **That merged wave was built and
run rather than reasoned about**, and it produced:

```text
  [1] JpaProbe$Reserva @0x12 <fields unreadable>
```

Two failures in one line. `<fields unreadable>` is `INVALID_FIELDID` reported honestly — the JVM does reject
a foreign field id, which was worth confirming. But the row is *also* labelled `Reserva` when it is an
`Itens`: a caller is told the wrong type with nothing indicating anything went wrong. The silent half is the
reason the assertion is on the positive property (`skus=`, a field of one type and of nothing else) and not
only on the absence of the error string.

### The measurement, and why the honest number is two numbers

Through `LatencyRelay` at a fixed 8ms RTT, dial-don't-restart, arms alternated, each scored on its fastest
sample (TEST-13, #38). The instrument is a **marginal** cost: the same query timed at 2 rows and at 50, the
difference divided by 48, then the same again at two round trip times and subtracted — which removes the
query's fixed cost and our own per-packet cost, leaving what the wire charges per row.

| rows of | before (sequential) | after (independent reads) | |
|---|---|---|---|
| `Bare` — a `long` and a `double` | **17.67 ms/row** | **1.78 ms/row** | 9.9x |
| `Reserva` — the realistic entity | **79.19 ms/row** | **63.18 ms/row** | 1.25x |

Both were measured. **Quoting only the first would be the mistake #100 warns about**, and quoting only the
second would hide that the primitive does what it claims.

The `Bare` row costs exactly its own two reads, so 17.67ms is the serialised `2 × RTT` (16ms predicted) and
1.78ms is the waved `2 × RTT / 16` (1.0ms predicted). The gap between 1.0 and 1.78 is the window's edges and
the per-wave fixed cost; it is quoted rather than rounded down to the prediction.

The `Reserva` row costs about **eight** round trips, and this change converted two of them. The other six are
`render_value` reading each `String` field with a `StringReference.Value` and each association with an
`ObjectReference.ReferenceType` — per field, per row, still serialised, and not this call site's to fix:
`render_value` is shared by every tool that prints a value. So the tool improved by a fifth, the primitive by
ten-fold, and the difference between those two numbers is a **follow-up**, not a disappointment.

That is also why the assertion in the measurement test is against `Bare`: a threshold set on the `Reserva`
figure would be a number between 63 and 79 with no meaning of its own, and would move the moment anything
else about rendering a row changed.

## What the tests hold it to, and the control that rewrote one of them

`wave_peer` reads a whole wave of commands **before answering any of them**. That is the instrument: a client
that awaited each reply before sending the next command can never get past its first command, so the peer
cannot be satisfied by the serialised path at all. Every wave test is wrapped in a budget, because an
assertion that hangs proves nothing until something reports it.

Each reply's payload is its own packet id. Asserting on the reply's id alone would pass a routing table that
delivered the right envelope with the wrong letter inside — ADR-0034's conflation in another form — so the
payload has to name its request too.

**The first version of the correlation test was worthless and looked strong.** It withheld "one window's
worth" of commands, which reads like the strongest available demand and is the weakest: a peer withholding a
window's worth is satisfied by a window of *one*, and with one command outstanding "the replies arrive
backwards" describes nothing. This was found by running the negative control — setting the window to 1 to
watch the test fail — and reading the output: it passed. The wave tests now demand a literal `WITHHELD = 8`,
a number they hold the implementation to rather than read off it, and the peer asserts loudly if the window
is ever lowered below it instead of deadlocking.

## The second call site: a stack walk, where deduplication mattered more than waving

`debug.get_stack --include_variables` made **three reads per frame** — the method's line table, the method's
variable table, and the frame's values. It is licensed as three waves, in that order, because the third needs
the slots the second produces.

**The bigger finding was not the waving.** The first two reads are keyed by `(class, method)` and the walk was
making them per *frame*. `debug.thread_dump` has cached line tables that way since TEST-8 (#24) — 300 workers
60 frames deep asked ~19,000 times for ~60 distinct tables — and `get_stack` never got the same treatment. So
this commit removes packets as well as waiting, and the two savings are independent of each other.

Measured on `StackWaveProbe` (66 frames of one recursive method, primitive locals only), warm, by recording
the session and counting the second walk:

```text
                                before   after
  Method.LineTable                  66       6
  Method.VariableTableWithGeneric   66       7
  StackFrame.GetValues              63      63
  ObjectReference.ReferenceType     12      12
  total commands                   209      90
  round trips (8ms RTT)          227.1    28.1
```

`GetValues` is unchanged and must stay so: one read per frame either way, issued four waves instead of
sixty-three times. **The twelve `ObjectReference.ReferenceType` reads are the largest single remaining item**,
and they are `render_value` resolving each object local's type one at a time — PERF-2 (#129) again, found by
census rather than by arithmetic.

Two tests, because the two savings are visible to different instruments:
`a_warm_stack_walk_reads_each_methods_tables_once` counts commands through the cassette recorder, which is
deterministic and load-independent as the house metric requires;
`a_deep_stacks_per_frame_metadata_costs_a_bounded_number_of_round_trips` times the wire, which is the only way
to see the waving at all.

### The prefetch is refused on the deep path, for two reasons that are both real

`expand_objects` renders values by invoking `toString()` in the debuggee, and **JDWP invalidates every frame id
on a thread when a method is invoked on it** — `render_frame_variables` already re-reads the frame id per frame
because of this. A wave of frame reads built before the walk starts would be reading stale ids by the second
frame. Separately, the deep walk **stops** when its shared node budget runs out, so a table read up front is a
packet the sequential walk would never have spent on a frame it never reached.

That second reason is the one that also decides the filter. A `package_filter` collapses frames without reading
anything about them, so the walk resolves the filter **first** — which costs nothing, because every frame's
class name is read either way — and prefetches only the survivors. **Speculation is the one way this change
could cost more than the loop it replaces, and it is the property every one of these conversions protects.**

## The third call site: a dump's triage, where a caller-facing model had to be amended

`triage_dump_threads` read a thread's **name** and **status** one at a time, for every thread the VM has — 306
on a production-shaped instance, and under the suspension. It is now two waves per window of sixteen threads,
with the name filter between them, because a thread whose name is filtered out never had its status read and
must not start being read now. `collect_thread_rows` behind `debug.list_threads` has the same shape and the
same fix.

**Chunked by `INDEPENDENT_READ_WINDOW` rather than handed all 306 ids**, which is the only reason that constant
is public. A dump's suspension budget is what bounds the freeze and it is checked between threads; nothing can
interrupt one `read_independently`. Chunking hands the budget back every window and costs it nothing in time —
a window of sixteen takes about as long as one sequential read — so the budget is checked as often in *time* as
before, and a sixteenth as often in *threads*.

Measured on a 20-thread dump at a 4ms round trip, through `LatencyRelay`:

```text
  packets              763  ->  763   (unchanged, as always)
  held (VM frozen)   3731ms -> 1175ms  (-68%)
  reported per packet 4.89ms -> 1.54ms
```

**The freeze is a third of what it was, and that is the payoff #100 says matters most** — more than the latency,
because a shorter suspension is less time a shared instance is stopped.

### What this cost: `held ≈ packets × (our processing + RTT)` is no longer true

That model is TEST-8's and ADR-0011's, and the dump **reports the per-packet term to callers**. It was exact
while every packet was awaited on its own. It is now:

```text
  held ≈ round_trips × RTT + packets × our processing
```

`latency_added_to_the_wire_shows_up_as_held_time_per_packet` failed on this, at 1.45ms against a floor of
1.6ms — which is how the amendment was found rather than predicted. Its assertion now divides by round trips
instead of by packets, which restores the linearity it was written to demonstrate against the denominator that
actually carries the RTT.

### So a cost line reports both numbers

`Cost: 763 JDWP packet(s) in ~180 round trip(s), 1.54ms each`. Both, because they answer different questions: a
caller reasoning about a remote instance needs the waits, and a caller comparing releases needs the traffic —
the packet count stays deterministic and load-independent, which is why it was not replaced. The clause is
**suppressed when the two are equal**, since printing one number twice as two facts is worse than printing it
once; seeing the clause is itself the information that something overlapped.

`JdwpConnection::round_trips()` is derived from the window and not observed on the socket — a single read counts
one, a wave of `n` counts `ceil(n / 16)` — so it is a tight lower bound rather than a measurement, and every
reply that prints it prints a `~`. This is a caller-visible reply change and `docs/toolkit-contract.md` carries
it as the "Change what a reply says" row, including the warning that the *smaller* per-packet number is the dump
getting faster rather than a regression.

## Consequences

- `issue` / `InFlight` / `read_independently` are public API on an unpublished crate. `scripts/semver-check.sh`
  compares against the last release tag; additions are not breaking, and relaxing `send_command` to `&self`
  later would not be either.
- The relay charges coalesced traffic once, which is exactly what a wave produces, so every figure above is
  a **lower bound on the real saving** and was read with that in mind: the assertion is set at one whole
  round trip per row, which a serialised path could not reach even with the flattery.
- `CONTEXT.md`'s `Packet` entry is amended in the same commit as the conversion, because that is the commit
  where the packet count and the round trip count stop being the same number. The cost lines still report
  packets — deterministic, comparable between releases — but on a converted path a packet figure is now an
  upper bound on what a caller waits for rather than a proxy for it.
- **The remaining per-row cost of `debug.run_named_query` is a follow-up**: six of a `Reserva` row's eight
  round trips are `render_value` reading a `String` or an association, per field, per row. Converting those
  means giving `render_value` a wave-aware form, which is a change to a function every tool shares and
  therefore its own piece of work rather than a widening of this one.
- No tool description changed, and no reply changed, so `docs/toolkit-contract.md` needs nothing beyond the
  release note. `Bare` and `Reserva.mixedTypes` are additions to a test probe, not to the tool surface.

## Alternatives considered

**`try_join!` over `&self` futures, with no new primitive.** The concurrency would have been real and the
code shorter. Rejected on two counts: the arity is fixed at compile time and every read #100 names fans out
over a runtime `n`; and first-error-wins would abandon waits for work the JVM does anyway. A dynamic
`join_all` would have meant a new `futures` dependency for a combinator the issue/await split makes
unnecessary.

**Unbounded in-flight commands, bounded only by the existing `mpsc::channel(32)`.** That channel bounds
*queued* commands, not outstanding ones — `pending_replies` has no bound — so this would have left both the
deadlock cycle and the reply-memory ceiling open, and made the fan-out of the caller's list the only limit.

**A second connection for concurrent reads.** Sidesteps correlation entirely and costs a handshake, a second
`TypeCache`, and a second thing for the JVM to suspend against. It also would not have worked: `dt_socket`
with `server=y` serves one handshaked session at a time (measured on JDK 11/21/25 in TEST-20, #55).
