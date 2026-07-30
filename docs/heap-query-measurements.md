# The JDWP heap-query family, measured

Wire details and **measured cost** for four JDWP commands this client does not implement, plus the full
`CapabilitiesNew` vector. Written down because the headline result is counter-intuitive and expensive to
reproduce: it needs a JDK, a shaped heap and a ticking probe.

Measured 2026-07-30 against `openjdk version "17.0.20" 2026-07-21, Temurin-17.0.20+8`, driven by
`jdwp-client/examples/probe_heap_queries.rs` against `HeapProbe.java` (7 `Widget`, 2 `SubWidget extends
Widget`, 1 `Target` in a `static final` field with 3 deliberate `Holder` referrers, 3 deliberately
unreachable `Widget`s, and N `Ballast` objects to size the live heap).

## The headline: these commands stop the world, and JDWP never says so

**`ReferenceType.Instances` and `VirtualMachine.InstanceCounts` pause application threads for the length
of a full live-heap walk** — even though neither requires a suspend, and the debugger issues none.

The probe ticks every 50ms and prints the measured gap; a **tick** is the only evidence a thread is
actually running (the debugger reports success either way). Correlating tick gaps against the wall-clock
window of each call:

| live heap | `Instances(Widget)` → 7 objects | worst application-thread tick gap |
| --- | --- | --- |
| 2,000,000 ballast objects | 57 ms | **522 ms** |
| 20,000 ballast objects | 4 ms | 54 ms (baseline is 50) |

Both runs returned **the same 7 objects**. So the cost tracks the **live heap**, not the result — and on
the 2M-object heap every tick during the sweep landed exactly on a call's end boundary (`…550356`,
`…550414`, `…550491`, `…550554`, `…550612`, `…550671`, `…550730` — precisely the probe's own `t1` values).
The ticker was held for the walk and released the instant it finished. That is not sampling noise.

`InstanceCounts` over 2M objects: **630 ms**, with a matching **522 ms** tick gap. Asking for three types
in one request cost **604 ms** — about *one* walk, not three, so batching types is close to free.

### What this means for the design

This is a diagnostic that looks free and is not. A WildFly heap on the shared 8180 is multi-GB, so a
single call could stall every in-flight request for seconds — the precise harm every safety default in
this codebase exists to prevent, arriving through a tool that suspends nothing.

So the feature is worth building — it answers questions nothing else can (see below) — but it must not be
sold as free. It needs the discipline `CONTEXT.md` already defines for **Held duration**: "the cost a
diagnostic imposed on everyone else using a shared instance". Concretely: report the measured pause the
way a traced stop point reports its own measured cost (ADR-0010), and make the blast radius part of the
tool description the way DOC-5 did for the six VM-wide tools.

## `ReferenceType.Instances` (set 2, command 16)

Request: `referenceTypeID` (8 bytes here), `int maxInstances`.
Reply: `int count`, then `count` × **tagged** `objectID` (1-byte tag + 8-byte id).

Measured tag bytes, all correct and distinct: `L` OBJECT, `s` STRING, `t` THREAD, `[` ARRAY,
`l` CLASS_LOADER, `c` CLASS_OBJECT. So the reply distinguishes a String from a plain object without a
follow-up round trip.

**`Instances` is EXACT-TYPE, not subtype-inclusive.** `Widget` answered **7**, not 9, while `SubWidget`
answered 2. This is the single most important semantic to get right in the tool: a caller who asks for
instances of a base class or an interface gets **nothing for the subclasses**, and on a CDI/EJB codebase
the useful name is very often the interface. A tool that does not say this will produce a confident
`0 instances` about a class with hundreds of live objects — the same "not loaded is not one of two honest
readings" trap `CONTEXT.md` records under **Loaded**.

Only **strongly reachable** objects are reported: the 3 deliberately-unreachable `Widget`s never appeared
(7, not 10), on every run and at every `maxInstances`.

`maxInstances`: `0` means all; a positive value clamps (asked 3 → got 3, asked 1 → got 1, asked 100 → got
7); **negative is `ILLEGAL_ARGUMENT` (103)**, both `-1` and `i32::MIN`. A bogus or `0` `referenceTypeID`
is `INVALID_OBJECT` (20).

## `VirtualMachine.InstanceCounts` (set 1, command 21)

Request: `int refTypesCount`, then that many `referenceTypeID`.
Reply: `int counts`, then that many `long`.

`refTypesCount = 0` is legal and answers `counts=0` in 0 ms. `-1` is `ILLEGAL_ARGUMENT` (103). A bogus
type id answers **`0` rather than erroring** — worth knowing, because it makes a typo look like an
absence. Costs one heap walk regardless of how many types are asked about (see the 604 ms three-type
result above).

## `ObjectReference.ReferringObjects` (set 9, command 10)

Request: `objectID`, `int maxReferrers`. Reply: `int count`, then tagged `objectID`s.

`TARGET` answered **4**: the 3 `Holder` instances **plus a `c` CLASS_OBJECT** — the class object that
holds the `static final` field. That is the useful shape for "why is this stale cache still reachable":
a static-field holder is visible and identifiable as such.

Clamping works but is **not stable in which referrer it keeps** — `maxReferrers=1` returned the
CLASS_OBJECT, not the first `Holder`. Negative is `ILLEGAL_ARGUMENT` (103); `objectID` `0` or bogus is
`INVALID_OBJECT` (20). Cheap on a small heap (3–5 ms) — but it is a heap walk, so expect it to scale like
the two above.

## `Method.IsObsolete` (set 6, command 4)

Request: `referenceTypeID` + `methodID`. Reply: one boolean byte. Answered `false` for a live `main`,
error 0. Would pair with `debug.reload_class`, which currently infers "frames still running old bytecode"
by comparing class ids instead.

## `VirtualMachine.CapabilitiesNew` — the full vector

The reply is 32 one-byte booleans. `jdwp-client/src/vm.rs` decodes only through position 11
(`canPopFrames`). Measured, in order, on Temurin 17.0.20:

| # | capability | 17.0.20 |
| --- | --- | --- |
| 1–7 | the `Capabilities` seven, repeated | all `true` |
| 8 | `canRedefineClasses` | true |
| 9 | `canAddMethod` | **false** |
| 10 | `canUnrestrictedlyRedefineClasses` | **false** |
| 11 | `canPopFrames` | true |
| 12 | `canUseInstanceFilters` | **true** |
| 13 | `canGetSourceDebugExtension` | true |
| 14 | `canRequestVMDeathEvent` | true |
| 15 | `canSetDefaultStratum` | true |
| 16 | `canGetInstanceInfo` | **true** |
| 17 | `canRequestMonitorEvents` | true |
| 18 | `canGetMonitorFrameInfo` | true |
| 19 | `canUseSourceNameFilters` | **false** |
| 20 | `canGetConstantPool` | true |
| 21 | `canForceEarlyReturn` | true |
| 22–32 | reserved | all false |

So `canGetInstanceInfo` (16) and `canUseInstanceFilters` (12) are both available, and
`canUseSourceNameFilters` (19) is **not** — a `SourceNameMatch` modifier would be refused on this JVM.

Note positions 9 and 10 being false is what makes `HotSpot`'s "method bodies only" restriction on hot
reload visible *before* a refusal, which is the rule `vm.rs` already states.

## Not yet measured

- **JDK 11 and 21.** Only 17.0.20 is covered above. The `capabilities` vector in particular is per-JVM and
  the whole point of decoding it is not to guess.
- **`ClassExclude` (modKind 6) and `InstanceOnly` (modKind 11)** on `EventRequest.Set`. The probe reaches
  this section and the connection closed on it (`early eof`) after `IsObsolete`, and one run ended with the
  JVM `Aborted (core dumped)`. **Unexplained and not dismissed** — it may be the probe writing a malformed
  modifier packet, which would be a bug in the probe rather than a finding about the JVM, but a debuggee
  dying is not something to write off. Needs a capture before either reading is asserted.
