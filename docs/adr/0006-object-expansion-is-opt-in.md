# 0006 — Object expansion is opt-in, because expanding invokes code in the debuggee

## Context

The original variable-inspection plan assumed objects would auto-expand to `max_depth=2`.

## Decision

Expansion is **opt-in** (`expand_objects:true`).

`debug.get_stack`'s default renders an object as `Type @0x…` (it was `Type (id=0x…)` until ADR-0022 made
the printed id a usable expression head) with **no invocation at all** — it passes no
thread, so nothing can run in the debuggee. `debug.evaluate`'s default **does** invoke: it calls `toString()`
to render the value, because "show me this value" is the whole request. Those are different defaults, and this
ADR originally described only the first as though it covered both.

Two consequences of that difference, measured against a real `WildFly` (EVAL-5, #23) rather than reasoned:

- **The shallow path is not always the cheap one.** `evaluate resp` on an Undertow response took ~40s;
  `evaluate resp {expand_objects:true}` took **4ms**. Expansion walks fields and invokes nothing per node,
  so opting into the "expensive" mode avoided the expensive call entirely. Anyone reasoning from "expansion
  is the costly option" will get this backwards on framework objects.
- **A rendering invocation therefore needs a budget of its own.** `INVOKE_SINGLE_THREADED` runs only the
  target thread, so a `toString()` needing a monitor held by another suspended thread cannot finish. It is
  now bounded by `DEFAULT_INVOKE_TIMEOUT_MS` (2s) and the expiry is **reported in the rendered value**;
  before that it waited on the event loop's generic 30s reply timeout — swept every 10s, so 30–40s of frozen
  VM — and then produced output byte-identical to the free shallow render.

Timing out does not cancel the invocation, because JDWP has no way to: the thread keeps executing and its
frames stay unreadable until it finishes or the VM is resumed. The reply says so, since the next thing the
caller sees would otherwise be an unexplained "no suspended frame".

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
