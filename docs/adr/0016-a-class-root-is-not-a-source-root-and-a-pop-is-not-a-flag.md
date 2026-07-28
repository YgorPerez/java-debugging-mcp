# 0016 — A class root is not a source root, and a pop is not a flag on a reload

## Context

SWAP-1 ([#58](https://github.com/YgorPerez/java-debugging-mcp/issues/58)) shipped hot reload:
`VirtualMachine.RedefineClasses`, fed with bytes read off the local disk. It left two calls open, said so,
and both are the kind that are cheap now and expensive later — one is a session-level configuration
surface, the other a tool name. DISC-7 ([#59](https://github.com/YgorPerez/java-debugging-mcp/issues/59))
depends on the first: it reads the same `.class` files to compare against, and its issue is explicit that
whatever #58 settles it must *reuse rather than answer twice*.

## Decision 1 — `class_roots`, a second list beside `source_roots`

**A new `class_roots` (plus `JDWP_CLASS_ROOTS` and a per-call override), not a reuse of `source_roots`
and not an explicit path per call.**

The three candidates were named in #58. Reusing `source_roots` is the tempting one: same shape, same
containment rules, same `<root>/<package as directories>/<file>` lookup, and the code is shared either
way (`find_under_roots`, `is_safe_path_segment`). It is wrong because the two lists name **different
trees**. A source root is `src/main/java`; a class root is `target/classes`. Nothing joins them — not a
convention, not a suffix, not a sibling directory — and Gradle, Bazel and a WildFly exploded war each put
the output somewhere else again. Overloading one list would mean either that configuring `debug.source`
silently configures a *mutating* tool, or that a caller has to list both trees under a name that says
"source" and hope each tool picks the right one.

An explicit path per call is not rejected — `class_file` exists, and it is the escape hatch for a build
output that is not laid out as a package tree. It is rejected as the *only* mechanism, on the same
grounds DISC-3 gave for `source_roots`: the checkout belongs to the JVM you attached to, not to the
question you are asking, and a path repeated on every call is a path that will eventually be wrong on one
of them.

The precedence rules deliberately copy `source_roots` exactly — call argument replaces session, session
comes from `debug.attach` or the environment, and roots given at attach *replace* the environment default
rather than adding to it. A caller who has learned one has learned both.

## Decision 2 — `debug.pop_frame` is its own tool, not `pop_frames:true` on `debug.reload_class`

#58 left this open too, noting that `debug.force_return` is "the closest existing sibling and its safety
framing is the model". Both halves of that turned out to point the same way.

ADR-0015 states the rule this settles: **a flag may change how an answer is bounded, filtered or rendered
— it may not change what the question was.** "Install these bytes" and "rewind this thread to the call
site" are two questions. They are usually asked together, which is an argument for making the second easy
to find from the first — not for hiding it inside the first's arguments.

Three things followed from ADR-0015's reasoning, applied here:

- **The name is the discovery mechanism.** Popping a frame to re-run a method you stepped past has
  nothing to do with hot reload, and a caller who wants only that would never think to read the argument
  list of a tool about class files.
- **The safety framing differs.** A reload is refused read-only because it installs code; a pop is
  refused because it changes what a running thread does next, and it is the *less* reversible of the two
  — whatever the popped invocation already wrote to a field, a file or the network stays written. Two
  refusals with two reasons read better than one tool with a compound one.
- **The footgun is handled by reporting, not by coupling.** The reason the pairing was proposed at all is
  that a swap of the method you are stopped in appears to do nothing. `debug.reload_class` therefore
  *checks* whether the target thread has frames in the redefined class and, when it does, names them and
  quotes the exact `debug.pop_frame` call. That solves the discoverability problem the flag was reaching
  for, and leaves the two questions separate.

## Consequences

`debug.check_stale` (DISC-7) reads `class_roots` and adds no configuration of its own, which is what the
two issues asked for. Anything later that needs compiled output — a bytecode-level comparison, a
`Method.Bytecodes` diff — inherits the same list.

A caller who configures `source_roots` and expects `debug.reload_class` to work gets a refusal that names
all three ways to set a class root and states, in as many words, that a class root is where the package
tree starts *in the build output*. That message is doing the work this decision costs, and it is asserted
in a unit test rather than left to drift.

One thing measured while building the pair, worth recording because it looks like a bug in the tool and
is the JVM being informative: after a redefinition, `HotSpot` reports a suspended frame's method with
**id 0**. `debug.pop_frame` renders that as an obsolete method rather than as `method@0` — it is the JVM
saying the frame is running bytecode the class no longer has, which is precisely the fact that justifies
popping it.
