# 0045 — Editing an armed stop point is its own tool, not arguments on `toggle_stop_point`

## Context

An armed stop point admitted exactly one in-place change: `debug.toggle_stop_point`'s `enabled`. Changing a
`condition` or a `hit_count` meant `debug.clear_stop_point` followed by a fresh `debug.set_*`.

BP-9 ([#159](https://github.com/YgorPerez/java-debugging-mcp/issues/159)) filed the cost, and it is three
things this repo has already paid to fix elsewhere:

- **The id changed.** BP-3 (#6) established a changing id as a caller-visible defect and fixed it for
  re-arming. Every condition edit reintroduced it.
- **The hit tally reset.** FILT-10 (#110) made that tally the thing a listing reports, and TRACE-15 (#156)
  made `Hits: 0` distinguishable from *fired constantly and was discarded*. Clear-and-re-set discarded the
  number both went to trouble to make trustworthy.
- **A `bpset_` family re-walked every loaded class** and re-installed its `CLASS_PREPARE` watch
  (ADR-0028) — the expensive half of arming, spent to change one string.

The loop this hurts is the ordinary one: arm broad, watch what fires, narrow. The narrowing is exactly
where you want to compare *how often the broad version fired* against *how often the narrow one does*, and
that was the number the old path deleted at the moment of comparison.

**The issue left one question open, and asked for it to be decided explicitly rather than by whichever was
fewer lines**: new arguments on `debug.toggle_stop_point`, or a tool of its own.

## Decision

**A tool of its own: `debug.update_stop_point`.** It takes `condition` / `clear_condition` and
`hit_count` / `clear_hit_count`, keeps the stop point's id and hit tally, and reports each changed field as
*old → new*.

Two arguments decided it, and the second is the one that would have bitten.

**ADR-0015's rule.** *A flag may change how an answer is bounded, filtered or rendered — it may not change
what the question was.* `toggle_stop_point` answers *silence this / put it back*. A condition answers *what
does this decide on*. The second is not a variation of the first. SESS-1 (#157) applied the same rule the
same week and reached the same shape.

**The name would have become a lie.** `toggle_stop_point` is named for the `enabled` flag. Folding condition
editing into it leaves either a tool whose name describes a third of what it does, or a rename — and a
renamed tool is the first row of `docs/toolkit-contract.md`, a break the downstream toolkit reads silently.
The principled argument and the practical one agree, which is why this is short.

### Three states per field, spelled out

`condition: "…"` sets it, `clear_condition: true` removes it, omitting both leaves it alone — and the same
pair for `hit_count`. Passing a field and its `clear_` flag together is **refused**, not resolved by
precedence.

Rejected: the idiomatic JSON encoding, where absent means *leave alone* and `null` means *remove*. It needs
`Option<Option<T>>` behind a custom deserializer, and — the deciding half — the published schema shows
`["string","null"]` either way, so the distinction between *remove it* and *leave it* lives only in prose.
The callers of this surface are models reading the schema. An extra argument is cheaper than an ambiguity in
the one place a partial-update surface is most often got wrong.

### What reaches the JVM, and what does not

**A condition change sends no JDWP packet.** Conditions are evaluated on this side, so it is a field write.
That is most of the value: the expensive family case becomes free.

**A `hit_count` change sends several.** `hit_count` is JDWP's `Count` modifier, which lives on the event
request and cannot be edited in place — so the request is cleared and re-set carrying the new count, by
replaying the existing `disable_stop_point` / `rearm_stop_point` pair that `debug.toggle_stop_point` already
uses. The id and the tally are this server's and survive; **the JVM's own count restarts**, because the
request holding it was replaced, and the reply says so rather than leaving it to be discovered.

Replayed through that pair rather than reimplemented, on the principle `debug.arm_stop_points` established:
refusals that exist in one place cannot drift from a second copy.

## Consequences

**A spent stop point is refused** (ADR-0026). It reached its count, the JVM has already deleted the request,
and `debug.clear_stop_point` sends nothing for one either. An update reporting success on a stop point that
can never fire again would read like a change and not be one.

**A deferred breakpoint IS editable**, which is where this parts company with toggling.
`enabled_state_of` refuses a pending breakpoint because it holds no request to silence — but it does hold
the definition it will arm with, and editing that is exactly what this tool is for.

**One of the issue's acceptance criteria had a premise that does not hold.** It lists monitor among the
kinds whose condition can be changed. `MonitorRequestInfo` has no `condition` field and
`debug.set_monitor_stop` takes no such argument — there is nothing to change. Its `hit_count` updates; a
condition is refused with that stated, rather than accepted and dropped.

**Location is not editable, deliberately.** A stop point at a different line is a different stop point, and
clear-and-set is the correct shape for it. Nor are `trace`, `trace_expr`, the trace bounds, or the
thread/instance filters — the last of those interacts with the `InstanceOnly` liveness check FILT-9 put on
the re-arm path, and folding it in here would mean two places deciding when an object filter is still valid.

**`toggle_stop_point` is unchanged**, and the two tools now divide cleanly: one decides *whether* a stop
point fires, the other *when*.
