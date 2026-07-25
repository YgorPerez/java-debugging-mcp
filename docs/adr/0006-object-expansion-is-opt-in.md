# 0006 — Object expansion is opt-in, because expanding invokes code in the debuggee

## Context

The original variable-inspection plan assumed objects would auto-expand to `max_depth=2`.

## Decision

Expansion is **opt-in** (`expand_objects:true`). The default renders an object as `Type (id=0x…)` with no
invocation at all.

## Rejected alternative

Auto-expansion, as planned. Expanding a collection means *invoking methods in the debuggee* — `toArray`,
`entrySet`, `getKey` — which needs a suspended thread and has side effects. Doing that for every local of
every frame by default would make `get_stack` slow and side-effecting on a JVM someone else is using.

## Evidence and consequences

Recorded in full in [`../VARIABLE_INSPECTION_PLAN.md`](../VARIABLE_INSPECTION_PLAN.md) — "Decisions worth
remembering" and "The type cache, measured" — including the node budgets, path-based cycle detection, and
the container-classification memo that was measured and dropped.

This ADR exists so the decision is findable from the ADR index rather than only inside a plan document, and
because it is the reason read-only output is necessarily shallow (ADR-0001).
