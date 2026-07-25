# 0001 — Read-only is enforced at the JDWP boundary, not by inspecting expressions

## Context

`JDWP_READONLY` / `attach {read_only:true}` exists so this debugger can be pointed at a production JVM
without a caller — including an agent — being able to change it by accident. `deleteAll()` is a valid
expression otherwise.

The first implementation (SAFE-3) guarded the *text* of the expression: a `(` at quote depth 0 meant "this
calls a method", plus a separate switch that disabled `expand_objects`.

## Decision

Read-only is a flag on `JdwpConnection`. The invocation and write primitives — `invoke_method`,
`invoke_static_method`, `set_reference_values`, `set_object_values`, `set_array_values`, `set_frame_value`,
`force_early_return` — return `JdwpError::ReadOnly` when it is set. The MCP layer does not decide what
counts as mutation; the wire does.

## Rejected alternative

Keeping the expression-text guard. It cannot be made complete, and the incompleteness was not theoretical —
SAFE-6 found four live bypasses, none of which contain a `(` in the user's expression:

| bypass | how it invoked |
|---|---|
| `debug.evaluate {expression: "order"}` | shallow rendering calls `toString()` in the debuggee |
| `order.lines[0]` | a `List` subscript calls `get(int)` |
| a boxed map key | `valueOf` |
| a breakpoint `condition` / `trace_expr` | evaluated on every hit, inside the event pump |

The first is the worst: `toString()` is user code and can do anything, and the expression looks like a
field read. The existing read-only test passed throughout, because it only tried field reads, an array
index, and an *explicit* `.getQty()`.

## Consequences

- Read-only output is **shallower**, and the refusal says so rather than pretending nothing is lost:
  objects render as `Type (id=0x…)`, because pretty-printing one means invoking it.
- Reads that need no invocation are untouched — locals, fields, statics, array indexing (`ArrayReference`
  reads invoke nothing), `get_stack`, watchpoint and exception reporting.
- An invoking `condition`/`trace_expr` is refused **when armed**, not silently on each hit, because a
  condition that fails to evaluate keeps the VM suspended and the caller never sees the error.
- The flag is shared with every clone of the connection, including the event pump's — which is what
  evaluates conditions.
- Still documented as a guard against accident, **not** a security boundary: anyone who can reach the JDWP
  port can open their own connection without it.
