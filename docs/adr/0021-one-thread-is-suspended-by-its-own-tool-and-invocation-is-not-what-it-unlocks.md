# 0021 — One thread is suspended by its own tool, and invocation is not what it unlocks

## Context

`ThreadReference.Suspend` (JDWP set 11, command 2) had a constant in `commands.rs` and **zero call
sites**, while its companions `resume_thread` (11/3) and `suspend_count` (11/12) were both implemented.
So the only way to obtain a frame the debugger could read was a whole-VM freeze: `debug.pause`, or a stop
point at `SuspendPolicy::All`.

This tool exists to be pointed at a shared `WildFly` instance other people are using. A whole-VM freeze
stops every in-flight request, including requests nobody told you about, so "needs a suspended thread"
was not an expensive capability there — it was an unreachable one. The concrete case from the stack
audit: reading `LayoutSrv.layoutLoginMap["ADTURISMO"]`, the commonest cache-staleness question in
integraWS. The standing workaround was to arm a stop point on an unrelated line purely to borrow a
thread.

SAFE-11 ([#90](https://github.com/YgorPerez/java-debugging-mcp/issues/90)) asked for a per-thread
suspend and named the capabilities it would unlock: `debug.evaluate` with a method invocation or
`expand_objects`, `set_value`, `force_return`, `pop_frame`, and a thread's own locks.

## Decision

### 1. Two tools, not a flag and not a fold into `debug.continue`

`debug.suspend_thread {thread_id}` and `debug.resume_thread {thread_id?}`, per ADR-0015's rule that **a
flag may change how an answer is bounded, filtered or rendered — it may not change what the question
was.**

- `debug.pause {thread_id}` would change the subject. `debug.pause` freezes every thread and its whole
  description, its idempotency and its watchdog coverage are about that; an argument that silently turns
  it into a one-thread operation makes one tool answer two questions.
- `debug.continue {thread_id}` fails the same test from the other end. `debug.continue` clears the
  **VM's** suspend depth. A thread's depth is a different count with a different remedy, and a caller
  who froze one worker on purpose and then continued past a breakpoint should not lose the worker they
  were reading.
- The name is also the discovery mechanism (ADR-0015's strongest argument). An MCP client sees a flat
  list of tool names, and `debug.suspend_thread` is what an agent looking for this will search for.

### 2. `debug.resume_thread` decrements **one** suspend and then asks the JVM

ADR-0003's rejected alternative was tracking our own suspend depth and resuming that many times, because
the count drifts the moment anything outside this session suspends the same thread. That argument
applies unchanged here, so the tool issues exactly one `ThreadReference.Resume`, reads
`ThreadReference.SuspendCount`, and says **`STILL suspended`** when the count did not reach zero. A
caller who suspended twice is told they are one call short instead of being told they succeeded.

Session state (`Suspensions::threads`) records *what this session asked for* and is never the authority
on whether a thread is running.

### 3. The watchdog covers per-thread suspends, on the same timer

A forgotten per-thread suspend is **less harmful than a whole-VM one and not harmless**: a worker frozen
inside a `synchronized` block holds its monitor for as long as we hold the thread, so every other worker
that needs that lock piles up behind it. That is a stall the caller never asked for, produced by the
*cheap* tool, and — before this — nothing else in the server would ever have resumed it.

It is a **separate branch** of the watchdog rather than an extension of the VM-wide one, because
`Suspensions::vm` means "the VM is stopped" and these threads are a different fact with a different
remedy. It releases only the threads that are *overdue*, verifies each against `SuspendCount`, and on
failure keeps the record so the next tick tries again — the SAFE-7 rule that a rescue must never go
quiet on a false success.

`debug.panic` and `debug.disconnect` clear them too. `panic` does it explicitly, because
`resume_all_fully` stops as soon as the thread it probes reaches zero and a held worker can sit above
that; `disconnect` gets it from `VirtualMachine.Dispose`, whose spec resumes thread-level suspends "as
many times as necessary", and the matrix asserts that against the probe's ticks rather than taking it on
trust.

### 4. **Invocation is not what a per-thread suspend unlocks**, and this is measured

This is the finding, and it contradicts the issue's own acceptance criterion.

Measured on JDK 21 against `SuspendProbe`: the same thread id that answers `ThreadReference.Frames`
with a full stack of readable locals answers **`INVALID_THREAD` (10)** to `ClassType.InvokeMethod`. JDWP
permits an invocation only on a thread suspended **by an event** — its own words are "Method invocation
can occur only if the specified thread has been suspended by an event. Method invocation is not
supported when the target VM has been suspended by the front-end."

Two consequences, and the second is the more useful one:

- `debug.suspend_thread` cannot make a Map subscript, a getter, `.toArray()` or a `toString()` work.
- **Neither could `debug.pause`, and that was true before this issue existed.** The refusals said "pause
  one or hit a breakpoint first"; half of that advice never worked, measured the same way. So the
  expensive remedy this issue set out to replace was not merely expensive — it was wrong.

What a per-thread suspend *does* unlock, all measured against the probe rather than inferred:

| capability | works on a thread you suspended? |
| --- | --- |
| `debug.get_stack` with locals | **yes** — full stack, locals rendered |
| `debug.evaluate` of a local or a field chain | **yes** (pass `frame_index`: a parked worker's frame 0 is native) |
| `debug.evaluate {expand_objects:true}` | **yes** — it walks fields, and reaches a `LinkedHashMap`'s **entries** as entries. When this table was measured it got there by accident: the deep path attempted `entrySet()`, the refusal below is what stopped it, and the fallback printed the map's `head`/`after` internals, in which the entries duly appear. [ADR-0046](0046-a-recognised-layout-is-walked-and-a-container-that-is-neither-walked-nor-invoked-says-so.md) makes the walk deliberate and first, and a container that is *not* one of the four walked layouts now says that what it printed is internals rather than contents (EVAL-15, [#179](https://github.com/YgorPerez/java-debugging-mcp/issues/179)) |
| `debug.set_value` on a local | **yes** — proved by the probe printing the written value |
| that thread's own monitors in `debug.thread_dump` | **yes** |
| method invocation of any kind | **no** — `INVALID_THREAD`; needs an event suspension |
| `debug.force_return`, `debug.pop_frame` | only if the thread is stopped in **Java** code; a worker parked in `Thread.sleep` has a native top frame and answers `OPAQUE_FRAME`, and so does every frame below it |

So the refusal text is split in two: `HOW_TO_SUSPEND` for reads, `HOW_TO_SUSPEND_FOR_AN_INVOKE` for the
operations that invoke. Merging them with a caveat is exactly how the old wording went wrong — one
sentence covering both cases has to be true of the stricter one to be true at all.

### 5. The suspension is visible, at zero JDWP cost

`debug.list_threads` marks the held thread on its own row with how long it has been held, and names one
that is held but off the current page; `debug.list_sessions` names them per session. Both read session
state, so neither spends a packet — which matters because `list_threads`' reply reports its own cost, and
because a `SuspendCount` per row on a 300-thread pool would be unaffordable.

`debug.list_sessions` deliberately does **not** fold this into its `SUSPENDED` state, which means the whole
VM is stopped and nobody's requests are served. A session holding one worker while the JVM serves
normally is a different fact with a different remedy.

## What the invoke timeout does and does not bound

The issue asked whether the existing invocation budget bounds a deadlock on a thread you suspended.
**It bounds the caller, not the debuggee** — `send_invoke`'s own doc comment already said so, and it was
confirmed here by accident: an invoke against a thread whose suspend count was 2 returned
`invocation did not return within 2000ms` after the full budget. `HotSpot` resumes the target thread once
to run the invocation, so a count above 1 leaves it suspended and the invocation never starts.

JDWP has no way to cancel an invocation. So the budget returns control to the caller with a reason
instead of blocking for 30-40s, and the outstanding invoke request stays on that thread until something
resumes it. A genuine monitor deadlock inside invoked code is not bounded by anything here; `debug.panic`
and the watchdog resume the thread, which is what lets the invocation proceed or fail.

## Rejected alternatives

- **A `thread_id` flag on `debug.pause`.** ADR-0015's rule, and the practical half: a caller reading
  `debug.pause`'s description would find a paragraph about freezing every thread and an argument that
  contradicts it.
- **Folding the resume into `debug.continue`.** Same rule from the other side. Note the two are not
  *fully* independent whatever we decide — `VirtualMachine.Resume` decrements every thread's count by
  one, so a `debug.continue` takes one suspend off a held thread as a side effect JDWP gives us no way to
  avoid. `debug.continue` therefore re-reads the JVM's count for every thread this session holds and
  names the ones still suspended, rather than pretending the two counts never touch.
- **Resuming by our own recorded count** (`issued` resumes in one call). This is ADR-0003's rejected
  alternative verbatim, and it also fails the issue's own criterion that suspending twice and resuming
  once must leave the thread suspended and say so.
- **Making `debug.suspend_thread` idempotent, as `debug.pause` is.** Pause is idempotent because a depth
  it built could not be cleared by one `debug.continue` and the watchdog would report a rescue it had not
  made. Here the depth is *reported* and the resume *verifies*, so refusing the second suspend would hide
  a state the caller can legitimately want — and it could not prevent a depth of 2 anyway, since a stop
  point or a `debug.pause` can supply the other one.
- **Leaving per-thread suspends out of the watchdog**, on the grounds that they are cheap. Rejected on
  the monitor argument above: cheap to take is not cheap to forget.
- **Escalating a stop point to `SuspendPolicy::EventThread`** so a single thread could be event-suspended
  and therefore invocable. That is FILT-7 and explicitly out of scope here — but it is the route by which
  the issue's original payoff could actually be delivered, and it is worth filing as such.

## Consequences

- The resume-honesty matrix is now **7 suspended states × 5 resume paths**: `Freeze` gains
  `ThreadSuspend` and `ThreadSuspendTwice`, `Resume` gains `ResumeThread`. Verified load-bearing by
  breaking this issue's own resume path twice and watching it name `(Step, ResumeThread)` and
  `(ThreadSuspendTwice, ResumeThread)`.
- `pending_step` became `(request id, thread)`. Releasing one thread with a step armed on it would
  re-stop it on the very next line — and a step event is `SuspendPolicy::All`, so the *per-thread* tool
  would have frozen the whole VM. That is a new way to leave the debuggee suspended, which is what the
  matrix's `Freeze` list is for; the `(Step, ResumeThread)` cell is what says so out loud.
- **Caller-visible** (`docs/toolkit-contract.md`): two new tools, and the "needs a suspended thread"
  refusals in `evaluate`, `set_value`, `force_return` and `pop_frame` changed wording — including one
  correction, since the old text named a remedy that does not work for invocation.
