# ADR-0030 — A redefine forecast states a refusal confidently and a pass weakly, and every prediction is checked against the JVM

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** DISC-13 ([#97](https://github.com/YgorPerez/java-debugging-mcp/issues/97))

## Context

`debug.reload_class` translates each of `HotSpot`'s twelve refusal codes into what to do next — but only
**after** the attempt. `debug.check_stale` answered "is the running bytecode older than my build" and said
nothing about whether the newer build could actually be installed.

Those are different questions, and the second one fails about half the time. Classifying the 300 most
recent `.java`-touching commits in `infotravel` over six months as structural (signature, field, import or
class-declaration change) versus body-only: **structural 151, body-only 149.** Essentially a coin flip.
The churn also concentrates where a redefine is most awkward — the largest JSF bean is 10389 lines with
349 fields and 216 commits in twelve months, and `ExceptionEnum` took 102 commits while being pure enum
constants, where *every* change is structural and can never be hot-reloaded.

Two failures also looked alike to a caller: **the deployed bytecode is stale** and **the redefine is
illegal**. The remedies differ — a redeploy or an overlay copy for the first, a restart for the second —
and in this environment the first is easy to hit by accident, because `mvn compile` does not update the
exploded war's `WEB-INF/classes`.

## Decision

**The two verdicts are held to deliberately different standards, and that asymmetry is the design.**

A **refusal is stated confidently**: `🚨 Redefine WILL BE REFUSED`, naming the JVMTI code and what in the
build produces it. A structural diff is decisive for six of the twelve codes —
`ADD_METHOD_NOT_IMPLEMENTED` (63), `SCHEMA_CHANGE_NOT_IMPLEMENTED` (64),
`HIERARCHY_CHANGE_NOT_IMPLEMENTED` (66), `DELETE_METHOD_NOT_IMPLEMENTED` (67),
`CLASS_MODIFIERS_CHANGE_NOT_IMPLEMENTED` (70), `METHOD_MODIFIERS_CHANGE_NOT_IMPLEMENTED` (71).

A **pass says only `NO STRUCTURAL CHANGE DETECTED`** and explicitly does not promise the swap will
succeed. The other six codes are not derivable from a static comparison — a verifier rejection (62),
`INVALID_TYPESTATE` against instances that already exist (65), a class-file version this JVM will not read
(68) — and `canAddMethod` / `canUnrestrictedlyRedefineClasses` vary by JVM (both `false` on Temurin 17;
see `docs/heap-query-measurements.md`). `debug.reload_class {"dry_run":true}` stays the authority on what
the VM can do, and the reply points at it. **A pre-flight that over-promises is worse than none**, so the
positive is the weaker sentence by construction rather than by hedging.

**Staleness and legality are reported as separate facts**, in separate sections of the same reply. They
are independent: a class can be both stale and illegal to swap, and a caller told only "stale" would pick
a remedy by guess.

**Every prediction is checked against a real `RedefineClasses`.** That is what makes the feature worth
trusting, and it is a test rather than a claim: `each_predicted_refusal_matches_the_code_the_jvm_answers`
compiles three variants of `SwapProbe`, forecasts each, then actually attempts the swap and fails if the
code the JVM answered is not among the predicted ones. Green on JDK 11, 17 and 21.

**A changed method signature is reported as both an add and a delete**, because that is what it is to the
JVM: one member gone, another arrived. All predicted refusals are listed rather than the first, since the
JVM stops at the first restriction it reaches and clearing that one can reveal the next.

### The modifier bits are masked, and the mask is the false-positive control

A false *refusal* is the expensive direction — it sends a caller to a restart that was not needed — so the
comparison excludes every flag a compiler or the JVM chooses rather than a declaration states:

| side | compared | excluded, and why |
|---|---|---|
| class | `public final interface abstract annotation enum` | `ACC_SUPER` (0x0020): every `javac` since 1.1 sets it and `HotSpot` normalises it internally. Comparing it unmasked reports a modifier change on **every class**. Also `ACC_SYNTHETIC`, `ACC_MODULE` |
| method | `public private protected static final synchronized native abstract` | `ACC_BRIDGE`, `ACC_VARARGS`, `ACC_STRICT`, `ACC_SYNTHETIC` — a difference there means the two builds came from different `javac` versions more often than it means someone changed a modifier |
| field | `public private protected static final volatile transient` | `ACC_SYNTHETIC`, `ACC_ENUM` |

**Both sides go through the same mask.** The first cut masked only the class-file side, which would have
made `ACC_SUPER` itself the difference on every class; it was caught before it ran.

Interfaces are compared **sorted**, on both sides. Neither JDWP's `ReferenceType.Interfaces` nor the class
file promises an order, and an order-sensitive comparison would report a hierarchy change on any class with
two interfaces.

### What is deliberately not predicted

Synthetic members are **kept in** the comparison rather than filtered out, even though a different-`javac`
deployment can make them differ. Filtering them would blind the forecast to the commonest surprise in this
area, which `explain_redefine_failure`'s own code-63 arm already warns about: a new lambda, an anonymous
class body or a new switch arm adds a synthetic method without looking like a new method in the source.
The residual false positive is stated in the reply instead of being silently traded away.

The class-file **version** is still not compared, holding ADR-era reasoning already in `classfile::parse`:
a file this JVM cannot load is refused by the JVM with `UNSUPPORTED_VERSION`, which is a better answer than
one derived from a number compared here.

## Alternatives considered

### A single boolean "can this be hot-reloaded"

Rejected. It would have to be either optimistic (and wrong half the time in the direction that costs a
wasted attempt) or pessimistic (and useless). The value is in *which* restriction and *what* tripped it —
that is the sentence a caller acts on.

### Report only the first refusal, matching what the JVM does

Rejected. The JVM stopping at the first restriction is exactly why reporting only one is unhelpful: the
caller fixes it, tries again, and meets the next. Naming all of them costs nothing, and the reply says the
JVM will answer with one.

### Put the forecast on `reload_class {dry_run:true}` instead

`dry_run` is the natural-looking home and was rejected as the *primary* one: it is a mutating tool's flag,
and the question "could this be installed" is a read that a read-only session and a caller who has not
decided to swap anything should both be able to ask. `check_stale` is already the pre-flight tool. `dry_run`
now says it does not compare shapes and points here, so neither tool implies the other's answer.

### Make it opt-in on `check_stale`

Rejected on cost. It is about six packets — class modifiers, superclass and its signature, the interface
list and one signature each, fields, methods — against the one-per-method the line-table walk already
spends. `get_methods` is cached, so that half is free. Opt-in would mean the caller has to suspect the
problem, which is the same mistake DISC-8 (#62) exists to avoid.

## Consequences

- `jdwp-client` gains `get_modifiers` (`ReferenceType.Modifiers`), deliberately uncached: a type's
  modifiers are what a redefinition is *about*, and a cache here would be a way to answer from before the
  swap.
- `classfile::parse` now reads the field table (it was skipped wholesale), method and class access flags,
  the superclass and the interface list. `parse` was split — `parse_header` — because the added
  resolutions pushed it past doctor's complexity gate.
- `explain_redefine_failure` says, on the six foreseeable codes, that the answer was available before the
  attempt. That is the half that changes behaviour: the caller who has just been refused is the one who
  most needs to know the question was answerable.
- The comparison is pure (`forecast_redefine` over two `ClassShape`s) so predictions are testable without a
  JVM, and normalised into one shape per side so a rule cannot come to hold for only one of them.
