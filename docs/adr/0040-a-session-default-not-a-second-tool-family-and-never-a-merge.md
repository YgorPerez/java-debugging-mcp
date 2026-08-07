# 0040 — A session default, not a second tool family, and never a merge

## Context

EVAL-14 ([#134](https://github.com/YgorPerez/java-debugging-mcp/issues/134)) named the shape it wanted:
*step, look at the same six things, step again*. `trace_expr` could not serve it, because since TRACE-11
([#93](https://github.com/YgorPerez/java-debugging-mcp/issues/93)) the list has belonged to the stop point
that declared it. Arming a second stop point elsewhere meant restating the expressions, and the two then
recorded into independently budgeted streams that had to be joined by hand.

The issue named both options rather than leaving the choice to be made by accident: a **watch tool family**
— `add_watch` / `remove_watch` / `list_watches` / `evaluate_watches`, which is what
[`kpanuragh/xdebug-mcp`](https://github.com/kpanuragh/xdebug-mcp) carries and where the issue came from —
or a **session-scoped default** for the `trace_expr` that already exists.

That whole surface is now sorted tool by tool in
[`docs/comparison.md`](../comparison.md#a-fourth-surface-in-another-language-kpanuraghxdebug-mcp), which is
where to start if you are comparing the two projects rather than reading this one decision (DOC-16, #161).
The watch family is one of twelve tools there that this repo has already settled against.

**This ADR exists mostly to be read next to [ADR-0015](0015-a-second-tool-not-a-flag-that-changes-the-subject.md).**
That one faced a comparable choice and decided the opposite-looking thing — a second tool rather than a
flag on an existing one — and its general rule is stated in as many words:

> a flag may change how an answer is bounded, filtered or rendered — it may not change what the question
> was.

A reader who finds both decisions will reasonably ask why this one went the other way. Without an answer,
the risk is not confusion but *correction*: somebody reads 0015, concludes this server prefers new tools,
and adds the watch family.

## Decision

**A session default.** `debug.attach` and `debug.launch` take a `trace_expr`; every stop point that names
none records that list. No new tools: 38 before, 38 after, 222 arguments to 224.

Four reasons, in the order they weighed:

- **ADR-0015's rule points here, not away.** That rule is about a *question*, and read carefully the two
  decisions are the same principle on opposite surfaces. 0015 rejected **one name serving two questions**
  — `list_methods {fields:true}` answering about fields. This rejects **two names serving one question**:
  "record these expressions at hits" already has a name, `trace_expr`, on all five stop-point tools. A
  watch family would be a second name for it, and the same argument that says a flag must not change the
  subject says a new tool must not duplicate one.
- **The discovery argument does not transfer.** 0015's strongest count was that the tool name is the only
  index a caller has, and it was not hypothetical: an agent went looking for `debug.list_fields` before
  anything by that name existed. Nothing comparable happens here. Nobody reaches for `add_watch` without
  having first met `trace_expr`, because the recording question is asked where stop points are armed and
  is answered there already. The name that needed to exist does.
- **One cost, so one budget.** Each expression is evaluated *inside the window a hit holds the thread*,
  which is why the list is capped at 4 and charged against the trace budget. A second family would need
  its own cap and its own accounting, and the two would have to agree about a cost paid in the debuggee.
  0015 wanted two independent budgets because the two answers were genuinely independent — a class's
  methods and its fields are separately bounded. Here there is one cost, paid once, and it has to stay one
  number.
- **Timing is a safety property, and a tool family names the wrong default.** An `evaluate_watches` tool
  implies watches that evaluate on demand, and the upstream shape evaluates at every event the server
  sees. On the shared instance this project is built around not disturbing, that spends debuggee time on
  events nobody asked to stop for. A session default has no such implication: it is resolved at arming and
  evaluated only at a stop the caller already caused, inside a capture that was going to happen anyway.

**And never a merge**, which is the second half of the decision and the part most likely to be "fixed"
later. A stop point naming its own `trace_expr` records exactly that list; the session's is not appended.
Merging looks harmless and is not: a caller who names four expressions is already at
`MAX_TRACE_EXPRS`, so any session list at all would push past the cap and the clamp would drop the tail —
silently from the caller's point of view, since they asked for four and four is what a reply would show.
The cap exists to make dropping *visible*; a merge would make the cap the thing that causes the loss.

## Rejected alternatives

**The watch tool family.** Rejected on the four counts above; the strongest is the third, because a second
budget for one debuggee cost is a disagreement waiting to be discovered on a shared JVM rather than a
duplication to be tidied later.

**Merging the two lists.** Rejected above. Worth stating as a rejection rather than an omission, because
"inherit *and* keep my own" is the intuitive reading of the feature and the reply had to be worded against
it.

**Auto-evaluation at every event.** Rejected: it is the upstream behaviour and it is the one shape that
spends debuggee time without being asked. Recorded here because the tool family and the auto-evaluation
travel together — taking the first invites the second.

**Storing the resolved list on the stop point at arming time** was not seriously considered but is worth
naming, since it is the obvious implementation: it would freeze the default at arm time, so changing the
session's list would leave already-armed stop points recording the old one, with nothing saying so.

## Consequences

**`inherited` is not the word.** It is already taken on this surface — `list_fields {inherited:true}` walks
the superclass chain, and it is ADR-0015 above that put it there. `#134` shipped it anyway for a day; the
glossary now carries **Session default** with the collision in its `_Avoid_` line. One word doing two
unrelated caller-visible jobs is the defect `CONTEXT.md`'s **Reply** entry was written for.

**Every arming reply says when it took the default**, and `debug.attach` reports the default when it is
set. A capture nobody asked for at a site they armed plainly is otherwise unexplained, and unexplained
output that reads as an answer is what this project treats as a defect rather than a rough edge.

**The list is clamped and read-only-checked once, at attach.** So taking the default cannot exceed the cap,
and a list that would invoke under `read_only` is refused where the caller set it rather than at the fifth
arming that would have taken it.

**One asymmetry in the implementation, which is not arbitrary.** `handle_set_line_stop` reads the default
through `session_trace_exprs()`; the other four stop-point handlers read `session.trace_exprs` directly.
Four of them already hold their session guard where the list is resolved, and that one clamps its arguments
before acquiring a session — calling the helper while holding the guard would re-lock the same mutex.

**Future session defaults land beside it.** `SessionDefaults` already groups `source_roots`, `class_roots`
and this; they are the same shape and the first two predate the phrase. BP-8
([#135](https://github.com/YgorPerez/java-debugging-mcp/issues/135)) is the next candidate and should be
weighed against this ADR before it adds a tool.

**It has been applied a second time, and the transposition held.** STEP-2
([#158](https://github.com/YgorPerez/java-debugging-mcp/issues/158)) gave the step filter a session default
— `step_exclude_classes` / `step_only_classes` on `debug.attach` and `debug.launch` — against xdebug-mcp's
`add_step_filter` / `list_step_filters` pair, which is the same tool-family shape this ADR rejected for
watches. All three rules carried over unchanged: a default and never a merge, resolved at the call rather
than frozen at attach, and every stepping reply says which list it used.

It needed **one clause added**, and that clause is the reusable part. The step filter is a PAIR of fields,
so "never a merge" had to be decided for the half-and-half case: a step naming `only_classes` alone does
not pick up the session's `exclude_classes`. Naming one field disables the whole session default, and the
built-in exclusion set fills the gap exactly as it would for a session that set none. That is the case a
merge looks most reasonable in, and the one where it would silently change where a step lands. The next
default of this shape should decide the same question before it ships rather than after.
