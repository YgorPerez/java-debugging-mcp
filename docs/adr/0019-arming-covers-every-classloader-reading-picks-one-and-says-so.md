# 0019 — Arming covers every classloader's copy; a read picks one and says so

## Context

A class name is not unique inside a JVM. `VirtualMachine.ClassesBySignature` returns **one entry per
classloader that has loaded the name**, each a separate reference type with its own methods, its own
field ids and — the part that bites — its own `static` state.

Six call sites in the MCP layer reduced that list to one and discarded the rest with no note. The most
consequential was the exact-named arming path, whose own comment called it "the overwhelmingly common
call": it took `classes.first()`. Searching the handler layer for `classloader`, `class loader`,
`duplicate class` or `ambiguous` returned nothing on the subject. The codebase nowhere admitted that a
class can be loaded more than once.

This is not a corner case on the stack this tool is pointed at. WildFly gives every deployment its own
module classloader; `it-common` and `api-common` are packed into **each** consuming war's `WEB-INF/lib`
with no shared module (`find $WILDFLY/modules -name 'infotera*'` returns nothing); and `infotravel.war`
and `integraws.war` are deliberately co-deployed into the same JVM, which the toolkit's own skills
instruct. So `br.com.infotera.common.util.Utils` genuinely exists as two reference types in one JVM, and
its `public static` non-final state — `aeroportoMap`, `tpAmbiente`, the endpoint URL strings — is a
different object per war.

Two symptoms, and the second is worse than a missing feature:

- **Arming.** You set a stop point on a shared-library class, the reply says armed, and it never fires,
  because it armed the other deployment's copy. Indistinguishable from a wrong hypothesis about the code
  path — which is the single most expensive thing this tool can hand a caller.
- **Reading.** Static field read with no suspended thread is the best-fitting capability this tool has
  for these libraries, and this bug made it *actively unsafe*: it answered confidently from whichever
  copy sorted first. `Utils.tpAmbiente = "H"` un-mutes five production-mute log handlers — in the war
  you may not have been looking at.

Same failure family as SIG-1 (#46), which `CONTEXT.md` records under **Loaded**: a class that is loaded,
sitting in the very list the tool just searched, and still missed. The rule stated there applies
directly — the tool must check before it blames, and a wrong answer is not one of two honest readings.

## Decision

**The two paths get different answers, because the two questions are different.**

**Arming arms every copy.** The caller asked about a class, not about a classloader; a stop point that
fires wherever that code runs is what they meant. The reply says how many (`armed on 2 classloaders`).

It stays **one** stop point: one `bp_` id, listed once, cleared once, its trace budget charged once per
hit. ADR-0005's one-id-per-stop-point rule is untouched. Mechanically this reuses `extra_locations`,
built one issue earlier for BP-4 (#78) — a `finally` body inlined once per exit path, which is the same
shape (one caller-facing stop point, N armed JDWP requests) with `class_id` held constant instead of
varying. Building the second as a parallel mechanism would have meant two disarm paths, two budget rules
and two ways to be wrong.

**Reading picks one, keeps today's choice, and says the choice was made.** `.first()` still wins when
nothing is specified, so nothing already scripted stops working. What changes is that the reply carries
a caveat naming every copy, and the caller can pin one.

**Selection is a suffix on the class name, not a tool argument** — `com.example.Utils@0x7f3a1c`. Five
read paths resolve a class name (`evaluate`, `list_fields`, `list_methods`, `source`, `check_stale`, and
`reload_class` makes six), a suffix composes with all of them at once, it travels through `trace_expr`
where there is no schema to extend, and it is copy-pasteable straight out of the list the caveat and
`list_stop_points` both print. **A selector that matches nothing is an error**, never a quiet fall back
to the first copy: being handed a different copy than the one you pinned is the exact failure the
selector exists to prevent.

A loader is named by reading its own reference type and signature — two ordinary JDWP commands, no
suspended thread. `toString()` is **not** called, so a WildFly `ModuleClassLoader` is named by its type
rather than by the module name it would have printed. That is a real loss of legibility, accepted
because an implicit invocation would violate the side-effect-free-by-default posture (ADR-0001), and the
`objectID` is what makes the entry actionable anyway.

## Rejected alternatives

**Refuse an ambiguous arm and make the caller disambiguate.** Honest, and wrong for the common case: the
caller usually wants the code to be watched wherever it runs, and on a co-deployed app server that means
every copy. It would turn the everyday call into a two-step and teach callers to paste a loader id they
have no way to choose between.

**Arm the copy that "looks right"** — most recently loaded, most instances, the one whose loader name
matches a deployment. Every heuristic here is a guess dressed as an answer, which is the thing this
codebase most consistently refuses. There is no signal in the JVM that says which deployment the caller
meant.

**Report the multiplicity and keep arming one.** This is the shape the bug already had, plus a warning.
A warning that names a problem the tool will not act on is what the caller has to work around, and the
workaround (arm by loader, one call per copy) is a capability that did not exist.

**A `class_loader` tool argument instead of a name suffix.** Would need adding to six schemas, would not
compose with `trace_expr`, and would leave the same question unanswerable inside an expression. Rejected
for reach, not for taste.

**Naming a loader with `toString()`.** It is what a human would want (`ModuleClassLoader for
deployment infotravel.war`) and it needs a suspended thread on a JVM where suspending is the thing being
avoided. The type name plus the `objectID` is strictly less readable and strictly more honest.

## Consequences

- `ReferenceType.ClassLoader` (JDWP 2/2) is implemented; its constant had sat in the command table with
  zero call sites. `Ok(None)` means the bootstrap loader and is a real answer, not a failure.
- Naming loaders costs three round trips per copy, so it is done **only** when the name resolved to more
  than one — the single-copy listing is byte-identical and pays nothing.
- A stop point stores the rendered loader labels at arm time rather than at list time, because
  `list_stop_points` is what a caller reaches for while deciding whether a trace is hurting a shared
  instance and must stay cheap.
- Loader `objectID`s are weak references and do not survive a redeploy. A pinned read after one fails
  loudly with the current list rather than silently reading a different copy.
- A copy of the class that does not resolve the requested line — two deployments carrying different
  builds of the same library — is *reported*, not dropped. "This deployment has a different build" is a
  finding.
- `ClassLoaderReference.VisibleClasses` (command set 14) stays unimplemented; it is a separate and much
  larger discovery feature, and nothing here needs it.
