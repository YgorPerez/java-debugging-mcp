# ADR-0029 — A source window is checked on two independent axes, and the timestamp half is labelled a hint rather than a verdict

**Status:** Accepted
**Date:** 2026-08-03
**Issue:** DISC-11 ([#87](https://github.com/YgorPerez/java-debugging-mcp/issues/87))

## Context

`debug.source` reads a `.java` off disk and shows a window around a line. It never checked that the file
corresponded to the bytecode the JVM had loaded, so it would render the wrong statement confidently — the
failure `debug.check_stale`'s own description exists to name ("a breakpoint that fires with locals that
make no sense looks identical to a wrong hypothesis"), arriving through the other tool.

**The issue prescribed a mechanism that cannot see the failure it measured, and that is why this ADR
exists.** Its Desired behavior says to "run the same line-table comparison `check_stale` uses". That
comparison comes from `handle_check_stale`: it reads the compiled `.class` from under `class_roots` and
compares its `LineNumberTable`s against the JVM's. It answers *is the deployed bytecode my build*.

But the issue's own evidence is that in the target environment the deployed bytecode **is** the build:

- `it-common`'s class root is 2 commits behind its `src/main/java`, `api-common`'s is 3.
- Both class roots are byte-identical to the deployed jars (1239/1239 and 598/598 classes identical).

The issue states this outright — "`check_stale` is trustworthy … and `debug.source` is not, for five
specific files". So on `WSIntegradorEnum.java` and the four others, a JVM-versus-build comparison reports
**a match**, `debug.source` stays quiet, and the caller reads a 259-supplier registry that has entries the
running JVM has never heard of. Implementing the brief as written would have shipped a warning that is
silent on exactly its own five named files: a green tick that verified nothing, in a repo built against
that shape.

There is a second problem underneath. Nothing on disk records which `.java` a `.class` was compiled from
beyond the `SourceFile` *name*, and this server does not run builds. So *source is ahead of bytecode*
cannot be proved the way *build is ahead of bytecode* can.

## Decision

**Two independent axes, reported as separate facts, because they have different remedies.**

| axis | evidence | strength | remedy |
|---|---|---|---|
| JVM's line tables vs the compiled `.class` | `compare_line_tables`, reused unchanged | proof | redeploy, or `debug.reload_class` |
| the compiled `.class` vs the `.java` being displayed | the two below | see below | recompile |

Collapsing them into one verdict would name the wrong remedy about half the time, and keeping them apart
is a property `debug.source` and `debug.check_stale` already go to some length to preserve.

On the second axis, two pieces of evidence of deliberately unequal weight:

**A length comparison, which is a proof.** The JVM's line table names the line numbers the compiler
emitted. If the highest of them exceeds the source file's line count, the file on disk cannot be what this
bytecode was compiled from — a file cannot be missing a line the compiler emitted an entry for. Decidable
without compiling anything. **Skipped when the class carries a JSR-45 SMAP**, because a translated class's
line numbers are positions in a `.jsp` or a template rather than in the `.java` that was resolved; without
that guard the strongest wording this check has would fire on every translated class.

**An mtime comparison, which is labelled a hint and says so in its own text.** A `.java` written after the
`.class` means the source is probably ahead of what is running, and in the measured environment it is the
only thing that detects the real failure. It is not a proof: a checkout moves a file's mtime without
changing a byte, so it can be wrong on a perfectly current tree. The wording therefore contains
"TIMESTAMP, NOT A PROOF" and names the checkout false positive, rather than being left to be read as a
verdict. It is suppressed when the length proof already fired — two warnings about one file read as two
problems, and the weaker one would be the memorable half. A slack of `SOURCE_MTIME_SLACK` (2 s) keeps
filesystem granularity from reporting every fresh compile as drift.

**Three outcomes, not two.** A matching build and a fitting source render **nothing at all** — the issue
is explicit that a correct reply must add no noise, and this tool's reply is read on nearly every call. A
check that could not run says `Freshness NOT CHECKED, which is not the same as checked and fine` and why.
Silence and cannot-tell are different answers, which is this repo's standing rule and matters here because
the common case in the target environment is no class root at all: the toolkit configures neither.

**Configuring a class root is what turns the check on, and that is the cost control.** The comparison
needs one `Method.LineTable` per method of the class, and `debug.source` is a tool a caller reaches for
constantly. Gating on `class_roots` means the packets are spent only where an operator has said where the
build output is; `class_roots: []` skips it. The tool description states the cost.

## Alternatives considered

### Implement the brief literally, and note the gap in a comment

Smallest diff. Rejected: it ships a detector that is silent on the five files named in the issue that
created it, and the next reader would have no way to know the tool answers a different question from the
one they asked. The failure is not that the brief was vague — it named its mechanism and its evidence, and
they contradict each other.

### One collapsed "stale" verdict

Rejected. A redeploy and a recompile are different actions, and a caller told only "stale" picks by guess.
The two axes are also genuinely independent: a class can be behind on either, both, or neither.

### Drop the mtime hint and ship only proofs

Tempting, and it is the conservative reading of the rule that a detector which cries stale on a current
build gets ignored within a day. Rejected because the length proof only fires when the source got
*shorter*, and the measured failure is a source that is 2–3 commits *ahead* — usually longer. Shipping
only proofs would mean the axis the issue was filed about is detected in the minority of cases. The
resolution is not to hide the weaker evidence but to label its strength honestly, which is what the
`TIMESTAMP, NOT A PROOF` wording does.

### Compare content instead of timestamps

There is nothing to compare. A `.class` carries no digest of its source, and the server does not compile.
The line table in the `.class` is the same table the JVM reports whenever axis one is clean, so it adds
nothing on axis two.

### Make it opt-in with a flag

Rejected for the reason DISC-8 (#62) gives for the arming caveat: the caller this failure ruins is the one
who does not suspect it, and a flag is only ever passed by someone who already does. `class_roots` is
already the operator-level statement of "here is my build output", so it carries the opt-in without adding
an argument a caller has to know to reach for.

## Consequences

- `debug.source` gains `class_roots`, replacing the session's per call, `[]` to skip.
- `local_source_section` now returns which file it read (`LocalSource`), so the check compares against the
  exact file that was printed. Resolving a second time would let the two halves land on different roots'
  copies and report a fact about a file the caller never saw.
- The verdict is a pure function (`source_freshness` over `FreshnessFacts`) for the reason
  `compare_line_tables` is: this is where a false positive would be born, and it has to be testable
  without a JVM or a file on disk.
- `debug.check_stale` is untouched. It remains the tool that answers axis one when *asked*, in detail, and
  the freshness note points at it.
