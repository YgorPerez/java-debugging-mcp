# Architecture Decision Records

Decisions that are **settled**, with the alternative that was rejected and the evidence that settled it.
Not proposals — `TODO.md` holds open work, and these are the record of what was already resolved.

Written because five review rounds kept re-deriving the same context, and one decision (0002) was nearly
re-proposed by the same agent that had rejected it a batch earlier. `docs/agents/domain.md` says to create
these lazily, when decisions actually get resolved; that point arrived.

| ADR | Decision |
|---|---|
| [0001](0001-read-only-enforced-at-the-wire-boundary.md) | Read-only is enforced at the JDWP boundary, not by inspecting expressions |
| [0002](0002-trace-budget-counted-server-side.md) | The trace-hit budget is counted server-side, not with JDWP's `Count` modifier |
| [0003](0003-suspends-are-counted-so-resume-must-verify.md) | Suspends are counted, so a resume must verify that the VM actually runs |
| [0004](0004-automatic-disarm-disables-rather-than-deletes.md) | An automatic disarm disables a stop point rather than deleting it |
| [0005](0005-stop-point-ids-are-not-jdwp-request-ids.md) | Caller-facing stop-point ids are independent of JDWP request ids |
| [0006](0006-object-expansion-is-opt-in.md) | Object expansion is opt-in, because expanding invokes code in the debuggee |
| [0007](0007-doctor-not-clippy-is-the-lint-gate.md) | `scripts/doctor.sh`, not `cargo clippy`, is the lint gate |

A root `CONTEXT.md` deliberately does not exist yet. The decisions were what had accumulated value; a
glossary had not.
