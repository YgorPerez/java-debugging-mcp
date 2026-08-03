# ADR-0033 — Generic types replace erased ones everywhere, and the fallback is the design

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** DISC-12 ([#95](https://github.com/YgorPerez/java-debugging-mcp/issues/95))

## Context

Four JDWP commands that supply generic signatures were in the command table and **never sent**:
`ReferenceType.SignatureWithGeneric` (2/13), `FieldsWithGeneric` (2/14), `MethodsWithGeneric` (2/15) and
`Method.VariableTableWithGeneric` (6/5). So a local showed as `java.util.List`, never
`List<ReservaHotel>`, and `list_fields` / `list_methods` erased every type parameter.

On a DTO graph that is the information a caller needs to compose the **next** expression. `api-common` is
almost pure DTO — 213 model classes, 88 request/response types, 60 more — and `it-common` adds ~105 domain
`WS*` types plus ~400 per-vertical ones. The workflow was: look at a frame, see a `List`, guess what is in
it, get an error, retry. The worst case found: `integraWS` holds a
`Map<Integer, Map<WSIntegradorEnum, LinkedList<WSSessao>>>`, which rendered raw reads as nothing navigable.

## Decision

### Replace the plain commands rather than adding an opt-in

`get_methods`, `get_fields` and `get_variable_table` now send the generic variants unconditionally, and the
generic type is what every renderer shows. The alternative — generics behind an argument — was rejected
because composing the next expression from one `get_stack` is the entire point, and a flag nobody passes
leaves the default at the guess-and-retry workflow the issue was filed about.

This changes type strings the tool emits, which is caller-visible and pinned downstream
(`docs/toolkit-contract.md`), so the release notes state it and the three tool descriptions say it.

### A full parser, in its own module

`crate::generics` is a complete JVMS 4.7.9.1 parser: type arguments, wildcards, type variables, nested types
that each carry their own arguments, type parameters with bounds, and `throws`. #95 offered rendering the
signature *verbatim* as an escape from writing one, and that was rejected: handing a caller
`Ljava/util/List<Lcom/x/Reserva;>;` would make the output *less* like Java, and the acceptance criteria
already require that nested and wildcard types not be mangled — which a parser has to handle either way.

Its own module rather than more of `handlers.rs`, because it is pure, it is the one piece of this change with
a grammar to get wrong, and it is worth being able to test to destruction without a JVM.

### Every function returns `Option`, and that is the whole design

A generic signature is an **optional** class-file attribute: absent for code compiled without it, absent
after erasure in some synthetic members, absent on arrays of type variables. The JDWP generic commands answer
with an **empty string** in that case rather than an error, so the naive implementation renders a *blank
type* — a regression on exactly the framework and generated code these codebases are full of.

Three layers make that unreachable. `jdwp-client` normalises the empty string to `None` at the wire
(`some_if_present`). `crate::generics` returns `None` for anything it cannot render **including trailing
garbage**, because a partial parse that dropped the tail would produce a *plausible wrong type*, which is
worse than none. And `shown_type` is the single home for the fallback, so a member with no generic signature
renders byte-for-byte what it rendered before DISC-12 — asserted directly, in the same reply as the
present case, because a fallback nobody exercised is a fallback nobody has tested.

### The two reply layouts are chosen by the same flag that chooses the command

`MethodsWithGeneric` inserts one string per entry between the signature and the modifier bits. Reading a
generic reply with the plain loop would take the generic signature *as* the mod bits and then desynchronise
for every remaining method — a silent, total corruption of the listing. So `read_methods`, `read_fields` and
`read_variable_table` each take one `with_generic` flag that picks the command **and** the layout, in one
function, rather than two loops that have to be kept in step.

### `get_stack` shows a declared type only where it adds something

`get_stack` never showed a local's type at all. Printing one unconditionally would change every locals line
in every reply for no gain — `i = (int) 3` says what `i` is. So the declared type appears exactly when the
generic rendering **differs from the erased one**: `java.util.List<Widget> lines = …` gains it, `plainCount`
and `plainText` are untouched, and one test asserts both halves in the same reply.

### `SignatureWithGeneric` is wired but not used for a value's type, and that is not an oversight

A *class's* generic signature describes its own declaration (`<T> ArrayList<T>`), not the arguments at a use
site. So `debug.evaluate` cannot show `List<String>` for a value: a runtime object's class carries no use-site
arguments, and there is nothing on the wire that would supply them. Generic types therefore appear where a
**declaration** is shown — a field, a method, a local — and not where a value is. `expand_objects` gets them
for free through `get_fields`, since a field tree is a list of declarations.

## Consequences

Verified on **JDK 11 as well as 17 and 21**, which the issue asked for specifically because
`VariableTableWithGeneric` is a JDWP 1.5 command and the oldest supported JDK is where a version-locked
assumption would surface. It works on all three.

The `NOT_IMPLEMENTED` fallback to the plain command is therefore for a non-HotSpot VM rather than for an old
JDK, and it is written down as such so nobody removes it believing it to be dead weight for JDK 8.

**Two cassettes had to be re-recorded.** They are recordings of this client's own traffic, and the client now
sends 2/14 and 2/15 — so `list_fields_disc5` and `list_methods_disc2` no longer had a matching reply and
failed with a `CASSETTE MISS` naming the new command, which is the mechanism working. The hand-edited
`method_exit_on_a_jdwp_1_5_vm` cassette needed no change.

One test caught a race in an **EVAL-9** test while this landed: it asserted a static read before the probe's
class had loaded, which passed on JDK 21 and 17 and failed *deterministically* on JDK 11. That is TEST-17
(#49) again, and the fix is the wait its two sibling tests already had.

## Alternatives considered

**Generics behind an argument.** No contract event, and no benefit either: the default would stay the
workflow the issue exists to remove.

**Emit both the generic and the erased name.** Nothing downstream that greps a raw type breaks, at the cost
of noise on every line of every listing — and the raw name is still one `decode_signature` away for anything
that needs it.

**Render the raw generic signature verbatim.** Cannot mangle anything because it parses nothing, and hands
the caller JVM-descriptor syntax to read. Rejected in the issue's own framing.
