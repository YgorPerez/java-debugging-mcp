# ADR-0026 — A spent stop point is reported spent, and clearing it sends nothing

**Status:** Accepted
**Date:** 2026-07-30
**Issue:** FILT-8 ([#99](https://github.com/YgorPerez/java-debugging-mcp/issues/99)), with
FILT-10 ([#110](https://github.com/YgorPerez/java-debugging-mcp/issues/110)) landed first

## Context

`hit_count` is JDWP's `Count` modifier. It does not mean "stop from the Nth hit onwards". It means the JVM
reports **only** the Nth occurrence and then **deletes the event request itself**. The stop point fires
once and is gone, and the deletion happens inside the debuggee with no event, no reply and no
acknowledgement of any kind.

Until this change nothing on this side tracked that. A line breakpoint armed with `hit_count: 3` that had
fired was listed as armed indefinitely, and `debug.clear_stop_point` on it sent a `Clear` naming a request
the JVM had already removed. FILT-8 set out to extend `hit_count` to the three kinds that never accepted
it — and found that the extension would have inherited both defects three more times rather than
introducing them.

The `CONTEXT.md` glossary already had the word for the state (**Spent**) and the constraint that makes it
matter (**Request id**): request ids are allocated by the debuggee and **recur**, so a stale id is not a
harmlessly dead number.

## Decision

**A stop point whose `Count` has fired is recorded as `spent`, reported as `SPENT`, and clearing it sends
no packet to the debuggee.**

Three parts, each load-bearing:

1. **`spent` is a third state, not a spelling of `enabled: false`.** Disabled is BP-1's toggle — something
   the *caller* did and can undo. Spent is something the *debuggee* did. Both end with no live request and
   both keep the definition so a re-arm reproduces it, so they share the re-arm path; only the wording
   differs. Reporting a spent stop point as `DISABLED` would tell a caller they switched something off
   that they never touched.

2. **The bookkeeping is exact, not heuristic.** `Count` means the JVM reports only the Nth occurrence, so
   the *first* event ever received for such a request **is** the Nth. Any hit on a stop point carrying
   `hit_count: Some(_)` therefore makes it spent — there is no counting on this side and no window in
   which the two could disagree about whether the request still exists.

3. **Clearing sends nothing, and says so.** No `Clear` packet, and the reply names the omission rather
   than reporting a plain success for work that did not happen.

## Alternatives rejected

**Always send the `Clear` and ignore the error.** This is what most debuggers do, and it is what the code
did before. It is specifically wrong *here* because request ids recur. A `Clear` naming a long-deleted id
can land on whatever now holds it — another stop point of ours, in the same session — and the failure is
silent in the worst direction: the caller's *other* breakpoint stops firing, which is indistinguishable
from a wrong hypothesis about the code path. That is the failure this whole codebase is organised against.
The error-swallowing makes it invisible: `Clear` on a live request succeeds, so there is no error to
ignore.

**Emulate "the Nth" by counting on this side.** Rejected because it is a different feature wearing the same
name. A server-side count of the *first N* already exists as `trace_max_hits` (ADR-0002), and a server-side
count of "only the Nth" would mean receiving every occurrence over the wire and discarding all but one —
paying the full debuggee cost of an unfiltered stop point to deliver a filtered one. `Count` exists
precisely so the JVM does that work.

**Delete the entry when it is spent.** Tempting, and wrong for the same reason BP-1 keeps a disabled stop
point listed: the definition — the condition, the trace expression, the count — is what the caller typed,
and losing it silently is a worse answer than a state they can read. It would also make "did my stop point
ever fire?" unanswerable, which is exactly what FILT-10 had just fixed.

**Refuse `hit_count` where it is awkward.** Considered for two cases and split:

- **Multi-location line stops** (a `finally` line, or a class loaded by several classloaders): JDWP applies
  `Count` per *request*, and one stop point can own several, each with an independent count. So it fires
  when whichever copy first reaches N, which is not "the Nth time the line ran". **Allowed**, because
  refusing would remove `hit_count` from any shared-library class on WildFly — the main target
  environment. The arm reply states the per-location semantics, and the survivors are cleared when the
  first copy spends, so nothing is left armed in the debuggee that this side can no longer match.
- **`hit_count` with `method` on a method-exit stop: refused.** Here there is no honest reading to
  document. A method-exit request is a `ClassMatch` firing for every method of the class, and `method` is
  filtered on our side afterwards; `Count` is applied by the JVM, before that. So `hit_count: 3` with
  `method: "save"` asks for exit number 3 of *any* method — almost certainly a getter — which this side
  then drops, leaving a stop point that reported nothing and that the JVM has already deleted. The
  refusal names the JDWP fact and points at `trace_max_hits`, which is what the caller usually meant.

## Consequences

- `debug.list_stop_points` gains a `SPENT` state with its own glyph (`⏹`). Callers matching on `DISABLED`
  to mean "not armed" now need both.
- `debug.clear_stop_point` replies gain a clause on the spent path. This is a changed reply on a tool
  downstream prose quotes (`docs/toolkit-contract.md`).
- The three arm replies that accept `hit_count` state what it buys **before** it fires: fires once, is then
  spent, and `trace_max_hits` cannot apply beside it.
- `debug.toggle_stop_point` re-arms a spent stop point with the same count, which is the way back and is
  named in the listing itself.
- **The retirement runs last in the hit path, and must stay there.** It was first — beside the hit tally —
  and that shipped two wrong numbers in one listing: `record_trace_cost` and `charge_trace_budget` both
  find their stop point by request id, and retiring it earlier left the cost reading "nothing captured
  yet" and the budget reading "200 hit(s) left" beside a real snapshot. The tally has no such dependency,
  which is why the two are separate calls rather than one.
