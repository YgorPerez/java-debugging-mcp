# 0050 — A session's stop-point bookkeeping is its own type, and which fields join it is decided by invariant rather than by touch count

## Context

`DebugSession` is a 35-field record. CLEAN-6 ([#189](https://github.com/YgorPerez/java-debugging-mcp/issues/189))
measured 324 direct field touches from `handlers.rs` against 40 mediated method calls — about 17 % of access
going through an interface — and asked for the collections to be closed cluster by cluster, each cluster
arriving with the unit tests that closing it made possible.

**One field makes the other 34 untestable.** ADR-0049 already established the shape: `JdwpConnection` has
exactly one constructor and it opens a `TcpStream`. So anything reachable only through a `DebugSession` is
reachable only from a test that launches a probe JVM, and `session.rs` had **7 unit tests for 75 public
items** as the honest consequence.

Two of those seven were worse than absent, and this is the finding that decided the shape of the change:

```rust
// Mirrors `next_stop_id` without needing a live connection to build a DebugSession.
let mut next = |prefix: &str| { seq += 1; format!("{prefix}{seq}") };
```

The test reimplemented the function in its own body and asserted on the reimplementation. `note_trace_disarm`
had the same shape. Neither could fail on any change to the real code — the **vacuous** verdict `CONTEXT.md`
defines, arrived at not by carelessness but because the alternative was a JVM.

**And an invariant was living in a handler.** The event pump resolved a **deferred** line breakpoint with
`pending_breakpoints.retain(…)` and `register_stop_point(…)` about ten lines apart, with a log line and — the
part that matters — an `await` between them. For the width of that await the breakpoint was in **neither**
collection: absent from `list_stop_points`, absent from the count, and absent from `owns_live_request`, which
is what decides whether a hit already in flight is surfaced or resumed and dropped. Nothing ever observed it,
because the caller holds the session guard across the whole block. That makes the invariant true by the
caller's good behaviour rather than by construction, which is exactly what #189 describes as *an invariant
that lives in whichever handler happens to update two fields together*.

`owns_live_request`'s own doc comment had been naming CLEAN-6 as the place its missing assertion belonged.

## Decision

**`SessionState` — a type holding a session's stop-point bookkeeping, constructible with no socket.**
`DebugSession` keeps `connection` and gains `state`.

**Which fields join it is decided by the invariants, not by the touch counts.** The methods that read or
write state together are `register_stop_point`, `resolve_pending`, `owns_live_request`,
`was_traced_and_disarmed`, `note_disarmed_traced` and `next_stop_id`, and between them they touch exactly
four fields: `stop_points`, `pending_breakpoints`, `disarmed_traced_requests`, `stop_seq`.

**`pattern_sets` is deliberately not among them**, though #189 grouped it with the stop points and it is the
second most touched field in the crate at 30 sites. No invariant here spans it. Moving it would have been
churn wearing this commit's clothes, and the measurement that would have justified it does not exist.

**`resolve_pending` takes the stop point and no id.** A deferral and the stop point it becomes carry the same
caller-facing id by definition — that is what makes `bp_4` still mean `bp_4` after the class loads (BP-3) — so
a signature taking both would invite a caller to pass two, and the pair that disagreed would remove one
deferral while registering a different stop point. Rewiring the pump also moved the `await` to *before* the
first write, so there is no window rather than a narrow one.

**The fields stay `pub`.** This commit builds the seam and moves the invariants; it does not add accessors.
#189's own rule is that *a getter that only returns a field is not mediation* — and the access shapes here are
`values` 13, `get` 9, `get_mut` 5, `values_mut` 4, so closing them would trade direct reads for indirect ones
and call it progress. Closing happens per cluster, where an invariant pays for the method.

## Rejected alternatives

- **`impl Deref for DebugSession { type Target = SessionState }`**, which would have kept ~64 call sites
  compiling untouched. Two reasons, and the second is the deciding one: it is the Deref-polymorphism
  anti-pattern, and it *defeats the purpose* — the fields would still read as living on `DebugSession`, so
  nothing about the seam would be visible at a call site and later clusters would have nothing to close.
- **`connection: Option<JdwpConnection>`**, so a test builds a session with `None`. It reaches the same place
  by making all 126 `session.connection` sites handle a `None` that cannot occur in production. A guard
  against a case the type admits and the program forbids is worse than the split.
- **A test-only second constructor on `JdwpConnection`** (an in-memory duplex, or a stated handshake). It
  changes `jdwp-client`'s public API for a test, and ADR-0001 enforces read-only *at the wire* with SAFE-12's
  `WIRE_COMMANDS` scan over it. ADR-0049 already declined the same move for the same reason — a fixture
  inside the type that holds the read-only guard is the "production job" CLEAN-2 refused for `send_command`.
- **Moving all 34 non-connection fields at once.** It delivers the seam for every cluster immediately, in one
  198-site diff. #189's US-9 asks for the opposite — *a 508-site sweep is never in front of me at once* — and
  its sprawl warning is the most load-bearing paragraph in the issue.
- **Naming it `StatedSession`**, joining `StatedClass` / `StatedObject` / `StatedDebuggee`. `CONTEXT.md`
  defines **stated** as a debuggee's answers *written down rather than recorded* — authored **test data**.
  `SessionState` is production state that happens to be constructible in a test, so the name would claim
  authority it does not have. This is the `TraceArm` error from #188 in a new costume: a name asserting a kind
  the glossary denies.
- **A `CONTEXT.md` entry for `SessionState`.** It is an internal type no caller ever sees, and the glossary is
  caller-facing vocabulary (DOC-17, #169).

## Consequences

- **`session.rs` goes from 7 unit tests to 12**, and one of the two mirror tests is now a real one:
  `stop_ids_are_sequential_and_prefixed` calls `next_stop_id` instead of reimplementing it. Mutating the real
  function to stop incrementing now fails that test; under the mirror it could not have.
- **`owns_live_request`'s debt is paid**, by `a_live_stop_point_of_every_kind_owns_its_request` — driven off
  `LISTING_ORDER`, so a sixth kind cannot dodge it — and `a_deferrals_class_prepare_counts_as_a_live_request`
  for the second clause. Its doc comment stops saying the test does not exist.
- **TRACE-8's rule has an assertion for the first time.** *Membership alone must never be the whole test*:
  a disarmed traced request stops matching once the debuggee reissues its id to a live stop point.
- **The deferral window is closed by construction**, and `resolve_pending` answers whether a deferral was
  really there — `false` in the event pump means two records are about to claim one id, and it is logged.
- **Later clusters have a home to move into**, so each is a smaller commit than this one.
- **Cost, stated plainly**: ~64 call sites read `session.state.<field>`, and a reader now has to know which
  half of the session a field lives on. That is the price of the seam and it is paid once per cluster.

## What resisted, and a correction to the issue

**Two of the three testing debts #189 is expected to pay are still open, and one of them was never as
described.** Recorded here because the amendment on #189 implied this change would clear all three.

- **#187's US-11 was already half paid, by work this ADR did not do.**
  `the_listing_groups_kinds_in_the_declared_order` asserts `in_listing_order` against stated stop points and
  needs no socket. What remains is the *renderer*, which still takes `&DebugSession`; the note at that test
  saying "#187's *a session built in memory* arrives with CLEAN-6" is still accurate, because a listing needs
  more of a session than its stop points.
- **CLEAN-7's shared assertion body is untouched.** ADR-0049's own "what resisted" says it needs *a session
  backed by a stated debuggee, which is `connection`'s seam and CLEAN-6's to own*. This change kept
  `connection` where it was and did not make it fakeable, so that is still open — and after the rejected
  alternatives above, it is not obvious it should be done by making `JdwpConnection` fakeable at all.

The honest reading is that the socket-free seam is **per cluster**, not a single wall that falls once. Each
cluster's tests become writable when that cluster moves, and a test needing two clusters waits for both.
