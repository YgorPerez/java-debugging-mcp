# 0001 — Read-only is enforced at the JDWP boundary, not by inspecting expressions

## Context

`JDWP_READONLY` / `attach {read_only:true}` exists so this debugger can be pointed at a production JVM
without a caller — including an agent — being able to change it by accident. `deleteAll()` is a valid
expression otherwise.

The first implementation (SAFE-3) guarded the *text* of the expression: a `(` at quote depth 0 meant "this
calls a method", plus a separate switch that disabled `expand_objects`.

## Decision

Read-only is a flag on `JdwpConnection`. Every primitive that mutates the debuggee — `invoke_method`,
`invoke_static_method`, `set_reference_values`, `set_object_values`, `set_array_values`, `set_frame_value`,
`force_early_return`, `redefine_classes`, `pop_frames` — returns `JdwpError::ReadOnly` when it is set. The
MCP layer does not decide what counts as mutation; the wire does.

**Amended by SAFE-9 ([#60](https://github.com/YgorPerez/java-debugging-mcp/issues/60)).** The last two
arrived with SWAP-1 (#58) gated in the MCP handlers that call them instead, which is what this ADR
forbids. There was no live bypass — those handlers were the only callers — but the invariant was broken and
nothing failed when it was, which is the whole failure mode this ADR exists to prevent. Two things came out
of the repair and are worth keeping:

- The guard was called `guard_invocation`, and **five of its seven call sites were already writes rather
  than calls**. A name narrower than the rule it enforces is how a new mutating primitive gets added
  without anyone noticing it skipped the guard; it is now `guard_mutation`. The error text lost "to
  invoke" for the same reason — it had been rendering "refusing to invoke a static field write".
- Read-only had **no wire-level test at all**: every assertion lived in the JVM-dependent integration
  suite, driven through MCP tool handlers, where a handler-level check satisfies the test and the missing
  wire guard is invisible. The refusals are now asserted against the connection API with no JVM, and on
  `packets_sent()` rather than only on the error — "refused" and "sent nothing" are different claims, and
  only the second is the contract.

**Amended by SAFE-12 ([#171](https://github.com/YgorPerez/java-debugging-mcp/issues/171)).** SAFE-9's
repair added two wire tests, one per repaired primitive — which closes yesterday's hole and leaves the
mechanism unchanged. Seven of the nine primitives still had none, and a tenth would have had none either.
**The invariant is now checked rather than described**, by two tables in `connection.rs`'s test module that
enumerate instead of listing:

| check | catches | would it have caught SAFE-9? |
|---|---|---|
| `mutating_primitives()` + `the_table_names_every_guard_mutation_call_site` | a guarded primitive with no wire assertion; a reworded or removed guard | no |
| `WIRE_COMMANDS` + `every_wire_command_this_crate_sends_is_classified` + `as_many_commands_are_classified_mutations_as_there_are_guards` | a mutating command sent with **no guard at all** | **yes** |

The second is the one that matters, because SAFE-9's actual defect was a primitive with no client-side
guard whatsoever, which leaves the set of `guard_mutation` literals unchanged and is therefore invisible to
any check built on them. `WIRE_COMMANDS` classifies **every** JDWP command this crate can send as `Read`,
`Mutation` or `AllowedStateChange`, so a new command turns the suite red until someone writes down which it
is — and once it is written down as a mutation, the count assertion turns it red again until a guard exists.
The third verdict is deliberate: `VirtualMachine.Suspend` is not a read, and filing it as one is how the
next classification gets made carelessly.

The cost is stated rather than discovered: adding **any** command to this crate, a read included, requires a
line in that table. Same deal as `docs_claims.rs` (DOC-15) — a red is fixed by updating the table in the
commit that caused it.

All four failure modes were verified by planting them: deleting either of two `guard_mutation` calls, adding
an unguarded mutating primitive, and adding a guarded one missing from the table. `VirtualMachine.CreateString`
is recorded in the table as `AllowedStateChange` — it allocates in the debuggee heap and is not one of the
nine primitives this ADR's decision names. That is the classification it *has*, written down so the question
is visible; changing it is a decision for this ADR, not for a test.

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
