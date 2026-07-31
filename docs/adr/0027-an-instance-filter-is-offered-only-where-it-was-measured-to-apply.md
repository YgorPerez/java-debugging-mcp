# ADR-0027 — An instance filter is offered only where it was measured to apply, and its arm reply states that it pins the object

**Status:** Accepted
**Date:** 2026-07-31
**Issue:** FILT-9 ([#101](https://github.com/YgorPerez/java-debugging-mcp/issues/101))

## Context

`InstanceOnly` (modifier kind 11) restricts an event request to occurrences whose `this` is one specific
object. It is the mechanism behind "trace `salvar()` on **this** `Reserva`, not all 400 in flight" — and on
**the shared 8180** the reason it is worth having at all is that it is a **filter** in `CONTEXT.md`'s sense:
the *debuggee* applies it, so a hit on any other instance costs no packet, no snapshot and no suspension.
A **condition** answers the same question and is paid for on every hit. Where a filter exists, it is the
one to reach for.

Wiring it up is mechanical. Two things about it are not, and both were settled by measurement rather than
by reading the specification, because the specification is silent on both.

### HotSpot accepts the modifier on kinds where it then ignores it

The JDWP specification lists `InstanceOnly` as valid on `BREAKPOINT`, `EXCEPTION`, `METHOD_ENTRY`,
`METHOD_EXIT`, `FIELD_ACCESS` and `FIELD_MODIFICATION`. HotSpot accepts it on all of them. It **applies** it
on some. Measured against one probe holding two live instances doing identical work in the same loop, with
the filter pinned to one of them (Temurin 17.0.20, 21.0.12, 25.0.3 — the table is identical on all three):

| target | armed? | actually filtered? |
|---|---|---|
| line stop in an **instance** method | yes | **yes** |
| line stop in a **static** method | yes | **NO** — fires for every hit |
| field watch on an **instance** field | yes | **yes** |
| field watch on a **static** field | yes | **NO** — fires for every write |
| **method-exit** on an instance method | yes | **NO** — records carry both instances |
| **exception** thrown from an instance method | yes | **yes** |

The two static rows are explicable — there is no `this` to match — though "explicable" is not the same as
"signalled", and the JVM signals nothing. The method-exit row is the one that decided this ADR: a method
exit **has** a `this`, so there is no structural reason for the filter not to work, the reply looks
entirely correct, and the request quietly records every instance. It was re-run on its own to be certain.

The exception row is the reason the table cannot be replaced by a rule. `EXCEPTION` also has a `this` and
**does** filter — 26 records over one run, every one of them the filtered instance, none from its twin
throwing the same type from the same line. So having a `this` predicts nothing, and neither does the
capability bit: `canUseInstanceFilters` reads `true` on the JVM where three of these six are inert.
`CONTEXT.md` names that state **inert** and states the rule it yields — *acceptance is not application*.

### An armed `InstanceOnly` filter pins its object

Found while building the vanished-filter reporting this issue's brief asked for, and it is the opposite of
what the brief (and `CONTEXT.md`'s **Filter** entry, now corrected) assumed. A JDWP object id is a weak
reference — ADR-0022 is built on that, and nothing in this codebase pins objects — so a filter naming a
collected object should simply stop matching, going quiet in a way indistinguishable from the code never
running. That is true of a `ThreadOnly` filter. It is **false** of an armed `InstanceOnly` one.

Measured with a probe that drops its last reference to one instance on cue and runs two `System.gc()`s,
against four controls (Temurin 17/21/25, identical):

| armed | object collected after the drop? |
|---|---|
| nothing | **yes** |
| a breakpoint on the same method, unfiltered | **yes** |
| the same breakpoint **with `instance_id`** | **no** |
| filtered, then **disabled** | **yes** |
| filtered, then **cleared** | **yes** |

So the modifier is the strong reference — not the stop point, not the handle, not the debugger — and
clearing or disabling the request releases it.

This trades one hazard for another rather than removing one. While armed, the filter cannot silently stop
matching, because the debuggee cannot collect what it is holding. But the stop point is now a **retention**
in the debuggee, holding the object and everything it reaches for as long as it exists. And the collection
hazard is not gone, only displaced onto the **disable → re-arm** cycle, which is exactly the workflow
`debug.toggle_stop_point` exists for.

## Decision

**`instance_id` is offered only on the shapes where the filter was measured to apply; every other shape is
refused up front with the JDWP fact that explains it; and the arm reply states that the filter pins its
object.**

Five parts.

1. **Refuse the three inert shapes, naming the measurement.** A line stop in a static method, a watch of a
   static field, and a method exit — the last unconditionally, since it is inert on every method. Each
   refusal ends with the same sentence, so a caller who hits any of them learns the rule rather than three
   restrictions that look unrelated. The method-exit refusal additionally says that the reply *would* have
   looked correct, because that is the part a caller cannot check for themselves.

2. **Refuse `instance_id` on a stop point for a class that is not loaded.** Not because it is inert — it
   cannot be evaluated at all. A deferred stop point arms on the event pump, where the static-method check
   above could be made but its result could not be reported to anyone. Refusing costs nothing real:
   `InstanceOnly` matches the hit's `this`, so the object would have to be an instance of that class (or of
   a subclass, which cannot load first), and an unloaded class has none — so a handle that parses here is
   pointing at something else.

3. **Allow it on `EXCEPTION`, on the strength of the measurement and nothing else.** This is the
   combination FILT-9 stopped on, and it was left unbuilt for a session rather than guessed. Allowing it if
   it did not filter would repeat the method-exit failure; refusing it if it did would have removed a real
   capability.

4. **Every arm reply states the retention.** One helper produces the line, so the fact cannot be stated on
   three kinds and forgotten on the fourth. This follows ADR-0010 and ADR-0023: a cost the caller is paying
   is reported at the moment they pay it, not designed away and not made a reason to refuse the tool.

5. **`debug.list_stop_points` and `debug.get_traces` report a vanished filter object, and a re-arm is
   refused.** The listing marks it loudly and the summary says why the silence is not "no hits" — the same
   treatment FILT-2 gives a dead thread filter, in its own sentence because the cause and the remedy
   differ. Re-arming is refused outright rather than warned about, since the alternative is a stop point
   that lists as armed and can never fire.

## Alternatives rejected

**Pass the modifier through on every kind and let the JVM decide.** This is what the code did before the
refusals, and it is the failure this repository is organised against: a reply saying a stop point is scoped
to one object while it reports all 400. There is no error, no warning and no later opportunity to notice —
the only signal is records from an instance the caller did not ask about, which they have no reason to
check for. A wrong answer delivered confidently is worse than a refusal.

**Allow it with a warning instead of refusing.** Considered seriously, and rejected on what the warning
would have to say: *"this filter will be ignored"*. A filter that is documented not to work is not a
feature with a caveat, it is an argument that does nothing, and the caller's only correct response to the
warning is to remove the argument — which the refusal does for them, in one step, at the same moment.

**Emulate it on our side for the inert kinds** — take every event and discard the ones whose `this` is
wrong. Rejected because it inverts the only reason to want a filter. The point of `InstanceOnly` is that
the debuggee does not generate the event; emulation pays the full unfiltered cost on the shared instance to
deliver a filtered-looking result, which is what a **condition** already does, honestly, under a name that
says so. Offering it under the filter's name would make the cheap thing and the expensive thing
indistinguishable at the call site — the exact confusion `CONTEXT.md` separates **filter** from
**condition** to prevent.

**Consult `canUseInstanceFilters` and trust it.** The bit is consulted, but only for the honest case: a
`false` bit means the JVM will refuse the request outright, and checking beats an `INTERNAL` (113) that
names nothing. It is **not** evidence that the filter will be applied — it reads `true` on the JVM where
three of the six shapes above are inert — and treating it as a guide is how a capability check becomes a
false assurance.

**Pin the object ourselves so the filter cannot go stale**, using `ObjectReference.DisableCollection`.
Rejected for ADR-0022's reason, unchanged: the debugger must never be why a live heap cannot be collected.
The irony is noted — HotSpot pins it anyway while the request is armed — but that pin is the debuggee's
own, released the moment the request is cleared, and bounded by the stop point's lifetime. A `DisableCollection`
would be ours, would survive a disable, and would outlive any listing that could tell the caller about it.

**Drop the vanished reporting now that an armed filter cannot lose its object.** Tempting, and wrong by one
step: the pin is released by a disable, and `debug.toggle_stop_point` makes that a first-class workflow
rather than an edge case. The reporting moved to where the hazard moved.

## Consequences

- `instance_id` is accepted on `debug.set_line_stop`, `debug.set_exception_stop` and `debug.set_field_stop`,
  and refused on `debug.set_method_exit_stop` — where it remains an accepted *argument* so that passing it
  produces the explanation above rather than "unknown field".
- A scoped stop point retains its object in the debuggee until cleared or disabled. On a shared instance
  that is a real cost, and the arm reply is where a caller finds out.
- The measurements are pinned by `a_stop_point_scoped_to_one_object_ignores_its_twin_and_refuses_where_it_could_not`
  and `an_armed_instance_filter_pins_its_object_and_reports_it_once_that_is_released`. Both assert against a
  **twin** or a **control** rather than against the filtered object alone, because "the filtered instance
  appears" is equally true of an unfiltered stop point, and "the object is still alive" is equally true of a
  collector that has not run. That asymmetry is what the method-exit row cost a session to discover.
- If a future JVM applies the modifier on the inert kinds, the refusals become wrong and the table is the
  thing to re-measure. It is one probe run per kind, and `InstProbe` is checked in for exactly that.
