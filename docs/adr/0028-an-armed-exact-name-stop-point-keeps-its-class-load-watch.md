# ADR-0028 — An armed exact-name stop point keeps its class-load watch for life, rather than reporting that it has gone stale

**Status:** Accepted
**Date:** 2026-07-31
**Issue:** BP-7 ([#115](https://github.com/YgorPerez/java-debugging-mcp/issues/115))

## Context

A class name is not unique inside a JVM. Every classloader that defines it produces its own reference
type — a **copy**, in `CONTEXT.md`'s sense. BP-5 (#79) made arming cover every copy loaded **at arm
time**, and BP-4 (#9) made an explicit re-arm re-resolve by name. Neither covers the sequence a
redeploy loop actually contains, because that sequence contains no re-arm at all:

```
set_line_stop            -> "Armed on 2 classloaders"
<edit java, mvn compile, cp classes, touch .dodeploy>
<fire the request that reaches the line>
get_traces               -> "No trace snapshots yet"
```

The stop point is still enabled and still listed. It is watching the **retired** deployment's copy. The
new deployment got a new module classloader, and nothing armed the class in it.

The mechanism was explicit in the code and is worth quoting, because it reads as deliberate:

```rust
/// The `CLASS_PREPARE` event-request id (cleared once armed).
pub class_prepare_request_id: i32,
```

Cleared once armed. So a deferred exact-name stop point watched for its class exactly **once, ever** —
and a stop point that armed immediately never registered a watch at all. A wildcard family keeps arming
classes that load later, by design (FILT-3). An exact name did not, **and a redeploy is precisely "this
class loads again"**.

What makes this worth an ADR rather than a patch is the *shape of the failure*, not its size. It is
**silent**, and it is indistinguishable from the hypothesis being wrong. Rule 0 pushes a caller to
`trace:true`, which by design produces nothing when it does not fire — so the natural reading of an empty
`get_traces` is "the code path I predicted is not the one running", and you go and re-read the code
instead of re-arming. It cost a full diagnostic cycle in the session that filed it, and the only tell was
the arming replies drifting across the session as retired loaders piled up:

```
Armed on 2 classloaders     (first arm)
Armed on 4 classloaders     (after two redeploys)
```

The same lingering copies produce a **loud** second symptom — a member lookup answered from the retired
copy, blaming the caller's signature — which is EVAL-13 (#116), fixed separately. One asymmetry, two
failures that look nothing alike. `CONTEXT.md` § **Copy** now names both.

## Decision

**An exact-name line stop point registers a `CLASS_PREPARE` watch on its signature and keeps it for the
stop point's whole life.** A copy of that class defined later is armed into the **same** `bp_` id.

Concretely:

* `BreakpointInfo` gains `rearm: RearmState` — `Watching(ReArmWatch)` / `CoveredByFamily` / `Unwatched`.
  The watch carries the live request id, the signature, and the location **as the caller asked for it**
  (`line_opt` / `method_hint`, not the resolved values).
* The deferred path no longer clears its watch when it arms; it hands it over.
* The immediately-armed path registers one, which it never used to.
* A wildcard family's member is `CoveredByFamily`: the family already owns one watch between them
  (FILT-3), and a per-member watch would arm every newly-loaded class twice.
* `clear_stop_point` and `debug.panic` clear it. A watch outliving its stop point would go on arming
  copies under an id the caller has been told is gone.
* `list_stop_points` reports the two facts **separately**: how many copies are armed now, and whether more
  will be.

**Three states rather than an `Option`**, because the two "no watch of my own" cases mean *opposite*
things to a caller. A family member is covered — a redeploy's copy matches the pattern and is armed as a
new member under its own `bp_` id — so telling its owner to re-arm would be false. A stop point whose
watch could not be registered genuinely does need re-arming. Collapsing them would have made the listing
print one of those two sentences about both.

### The location is re-resolved from the caller's request, not from the resolved values

`ReArmWatch` carries `line: Option<i32>` and `method: Option<String>` rather than re-using
`BreakpointInfo`'s `line` and `method`, which are what the *first* copy resolved to. A stop point armed
by **method name** has a resolved line; reusing it as the target would land the new copy wherever that
line number happens to sit in the redeployed class — drift the caller never asked for, arriving silently,
in the exact scenario where the class has just changed.

### The two facts are kept apart in the listing

```
     ↻ 1 copy/copies of this class have loaded SINCE it was armed and were armed too …
     👀 Watching for more copies — … does NOT need re-arming after a redeploy
```

"Armed on 4 classloaders" alone cannot distinguish a library packed into four wars from three redeploys of
one, and only the second reading means *a copy you care about may have arrived since you last looked*.
A stop point that is **not** watching says so explicitly rather than leaving it to be inferred from an
absent line — which is the case when a watch could not be registered, and it must not be
indistinguishable from the fixed one.

## Alternatives considered

### Make the failure loud instead of fixing it (the issue's own fallback)

`get_traces` and `list_stop_points` would report that a stop point's armed reference type is no longer
among the loaded copies of its name. **Rejected**, though it was a real option and is why this is an ADR.

It is strictly worse on the axis that matters and not obviously cheaper. Detecting staleness means
calling `classes_by_signature` for every armed stop point on every listing — a per-call cost on the tool a
caller reaches for *while deciding whether a trace is hurting a shared instance* — against a
`CLASS_PREPARE` filter evaluated **in the JVM**, on one exact signature, which a redeploy is the only
thing that makes fire. And it would leave the caller doing the re-arm by hand every time, on a loop whose
whole point is that it is fast.

It also cannot answer honestly in the case that matters most. The retired copy is *still loaded* — that is
the premise — so "your armed type is no longer among the loaded copies" is **false** exactly when the bug
bites. The honest version of the warning is "there are now more copies than you armed", which is one
`classes_by_signature` away from just arming them.

### A cheaper watch: no suspend policy

The watch uses `EventThread` suspend, matching the deferred path: the preparing thread is held so the new
copy is armed **before any of its code runs**, and the pump resumes that one thread. `SuspendNone` would
be cheaper and would reintroduce, on a smaller scale, the race the deferred path was built to close — a
class whose static initialiser runs the line you armed would fire before we got there. Rejected for the
same reason the deferred path rejected it.

### Re-arm on `check_stale` or on the next tool call

Rejected as a policy that fires at an unrelated moment. Arming is an action against the debuggee; doing it
as a side effect of a *read* is the implicit-invocation shape ADR-0001 exists to refuse.

## Consequences

* A stop point armed before a redeploy fires after it. The workaround this replaces — re-arm every stop
  point after every redeploy, or arm with a wildcard so the family's watch keeps arming — is no longer
  needed, though both still work.
* Every exact-name line stop point now holds one extra JDWP event request for its whole life. The cost is
  one filter evaluation in the debuggee per class load, against one exact signature — **a filter evaluation,
  not an event.** The request carries a `ClassMatch` modifier, so a class that does not match raises nothing:
  no packet, no suspension, no resume. An **event** costs those, and for an exact name only a redeploy of that
  class produces one.

  Spelled out because "per class load" was read as "an event per class load", which turns a per-redeploy cost
  into a per-classload one. That misreading reached the downstream toolkit's glossary, where it sat as the
  definition of half of its `arming cost` term, and from there into an argument for a per-load counter — which
  has nothing to count, since non-matching loads raise nothing and matching ones are already reported.
* Retired copies stay armed. That is deliberate: an undeployed module whose loader is genuinely
  unreachable takes its copy with it, and the case that costs time is the one where it does not. The
  listing says how many copies and how many arrived since, so a reader can tell.
* `debug.set_line_stop`'s description changes, which is caller-visible under
  `docs/toolkit-contract.md` and updated in the same commit.
* Proven by `a_stop_point_armed_before_a_redeploy_arms_the_new_classloaders_copy_too` against
  `RedeployProbe`, whose second copy loads **on a cue** rather than a timer — the arming has to happen
  while only the first copy exists, and a timer racing the arm would make a green run mean nothing.
  Defeat-the-fix confirmed: with the watch not registered, the test waits out the full `EVENT_TIMEOUT` and
  reports nothing, which is exactly how the bug presents in a real session.
