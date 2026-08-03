# ADR-0034 — A condition may name what the hit carries, not only what the frame holds

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** FILT-6 ([#83](https://github.com/YgorPerez/java-debugging-mcp/issues/83))
**Amends:** [ADR-0020](0020-a-conditional-stop-point-decides-on-one-thread-and-escalates.md) (the condition policy),
[ADR-0031](0031-an-escalated-trace-is-reported-not-refused.md)

## Context

`condition` existed on the line-stop argument struct **only**. Exception stops, field stops and method-exit
stops had none, and `parse_bool_tree` modelled `And` / `Or` / `Leaf` with no `Not`, so no condition anywhere
could be negative.

The exception case is the expensive one. In `infotravel`, `InfoTravelException` is simultaneously the error
type and the validation-control-flow type: **812 `ExceptionEnum` values, 247 of them validation**
(`documentoNaoInformado`, `emailJaCadastrado`, …), thrown as ordinary flow. An unfiltered exception trace
burns its 200-hit budget on validation noise before a real fault lands.

And the discriminator **cannot be the message**: `InfoTravelException(ExceptionEnum)` calls no `super(...)`
and never sets its message field, so `getMessage()` is `null` for **1104 of 3166** constructions. The only
usable discriminator is the `cdException` field — on the exception **instance**, which a condition evaluated
on the frame could not reach at all.

## Decision

### A condition may name what the hit carries, through reserved heads

An exception hit's top frame belongs to the *throwing method*, so `this` is the thrower and the exception is
not in scope anywhere. The exception is therefore bound to the reserved head **`exception`**, exactly as
`this` is a reserved head on a frame: `exception.cdException != ExceptionEnum.validarRegistro`.

A field hit binds **`newValue`** — the value the write is about to store, which reading the field cannot give
you, because `FIELD_MODIFICATION` is reported *before* the write lands.

**There is deliberately no `oldValue`.** At condition time the field still holds it, so the field's own name
reads it: `status != newValue` asks "does this write actually change anything", which is usually the question
and is more discoverable than a second reserved word. A binding for it would have cost a round trip per hit
to supply something already available.

A method-exit hit binds nothing: its frame is the returning method's own, so its locals and `this` are all in
scope already.

### An object binding is a handle rewrite; a primitive binding is used directly

An object binding is applied by rewriting the head to its `@0x…` handle, which is **already** a supported
expression head (TRACE-10) — so chained access through it (`exception.getCause().getMessage()`) needs no
change to head resolution at all. The rewrite fires only where the name is genuinely a head: at the start or
after a character that cannot continue an identifier, followed by a `.`, and never inside a string literal.
Both exclusions are unit-tested, because a substitution one character too wide corrupts a condition instead
of failing it — `exceptionCode == 1` becoming `@0x1fCode == 1` is a parse error about the wrong thing.

A primitive has no members to chain, so it cannot be a handle and is used directly when a comparison side is
exactly the bound name. That is the only shape a primitive can appear in, so the two mechanisms cover the
whole grammar between them without overlapping.

### `!` binds tighter than `&&`, which binds tighter than `||`

`parse_bool_tree` gains `Not`, applied **after** both splits so the precedence falls out of the order rather
than needing to be enforced. `!a && b` is `(!a) && b`; reading it as `!(a && b)` would silently invert half of
every condition it appeared in, which is a wrong answer nothing downstream can catch — so that is what the
test asserts, not merely that the node exists.

`!=` stays one operator: the negation branch declines when the character after `!` is `=`. The motivating
condition of the whole issue is `cdException != …`, so a `!` check that fired on it would have broken the
case the issue exists for.

The grammar is one grammar, so `!` works in a `[?pred]` filter too — `orders[?!paid]` — which costs nothing
to support and would have been surprising to refuse.

## Consequences

**The read-only refusal had to be extended, and it was a hole waiting to open.** `check_readonly_exprs`
already refused an *invoking* condition, and the three new sites passed `None` where the condition goes —
because there was nothing to check, not because a condition is exempt. Left alone, this change would have
opened three holes in `read_only` at once. A test now arms an invoking condition on all four kinds and
expects four refusals.

**A condition-skipped hit is still not charged to the trace budget**, for all four kinds — that behaviour was
already correct for line stops and the issue explicitly warned against rebuilding it. What the tests assert
is the arithmetic from both sides: no noise value in the buffer, and strictly more `Hits:` than captures.

**The cost is stated on all three tool descriptions**, per the issue: on a suspending stop point the VM is
frozen while the condition is evaluated. That was already true of line stops; extending conditions to three
more kinds widens its reach. Arming conditions at `EventThread` policy and escalating only on a match stays
out of scope, as the issue directs.

## What a defeat-the-fix run caught, and the lesson

Gutting the implementation exposed a **real defect** the tests had been passing over: `find_traced_request`
still read `condition: None` for all three new kinds, so on the **traced** path — the one the issue is
actually about — no condition was being evaluated at all. Every record was being captured.

The test had a negative assertion for exactly that and it could never fire: it matched `-> (int) 1` where the
renderer writes `new=(int) 1`. A needle checked against a guess rather than against a real reply.

So: **a negative assertion has to be seen failing before it is trusted.** The gutting run is what made it
fail, which is the whole reason the practice exists rather than being a formality. The test now also asserts
`Hits:` > captures, which is the same claim from a direction that cannot be spelled wrong.

## Alternatives considered

**A magic `this` rebinding on exception stops** — make `this` mean the exception. Rejected: `this` already
means the thrower, some conditions want it, and silently changing what a name means between stop-point kinds
is worse than adding one.

**Reading the exception's message and matching on it.** This is what a caller would reach for, and it is
exactly what the measurement rules out: null for a third of all constructions in the target.

**Binding the returned value on a method-exit stop.** The frame already has the locals it was computed from,
and `with_return_value` shows the value itself. A binding would have been a fourth reserved word for the
narrowest case.
