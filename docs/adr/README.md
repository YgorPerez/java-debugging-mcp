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
| [0008](0008-caller-frames-fetch-the-whole-stack-and-truncate.md) | Caller frames are read by fetching the whole stack and truncating |
| [0009](0009-thread-dump-suspends-only-when-asked.md) | `thread_dump` suspends only when asked, and verifies the resume |
| [0010](0010-a-traced-stop-point-reports-its-own-measured-cost.md) | A traced stop point reports its own measured cost, and the timer wraps the capture only |
| [0011](0011-line-tables-are-cached-per-dump-not-per-connection.md) | Line tables are cached per dump, not per connection — and the budget stays at 2000 ms |
| [0012](0012-one-task-owns-stdout.md) | One task owns stdout |
| [0013](0013-a-default-dump-selects-by-name-family-not-creation-order.md) | A default dump — and, since #51, a default `list_threads` — selects by name family, not by creation order |
| [0014](0014-a-cassette-is-keyed-by-the-request-and-a-miss-is-loud.md) | A recorded JDWP cassette is keyed by the request, and a miss is loud |
| [0015](0015-a-second-tool-not-a-flag-that-changes-the-subject.md) | A field listing is a second tool, because a flag may not change what the question was |
| [0016](0016-a-class-root-is-not-a-source-root-and-a-pop-is-not-a-flag.md) | Compiled output gets its own `class_roots`, and `pop_frame` is its own tool rather than a flag on a reload |
| [0017](0017-an-exception-message-is-read-as-a-field-except-the-one-the-jvm-never-stores.md) | An exception's message is read as a field, with one narrowly-gated invocation for the helpful NPE the JVM never stores |
| [0018](0018-the-socket-read-half-is-owned-by-its-own-task.md) | The socket's read half is owned by its own task, because `select!` cancels `read_exact` and JDWP cannot resynchronise |
| [0019](0019-arming-covers-every-classloader-reading-picks-one-and-says-so.md) | Arming covers every classloader's copy of a class; a read picks one, says so, and can be pinned |
| [0020](0020-a-conditional-stop-point-decides-on-one-thread-and-escalates.md) | A conditional stop point decides on one thread, and escalates to a VM-wide suspend only when the condition holds |
| [0021](0021-one-thread-is-suspended-by-its-own-tool-and-invocation-is-not-what-it-unlocks.md) | A per-thread suspend is its own pair of tools, and what it unlocks is every read of a frame — but not method invocation, which JDWP reserves for an event-suspended thread |
| [0022](0022-an-object-handle-is-printed-weak-and-never-pinned.md) | An object handle is the `@0x…` every reply prints, and it stays a weak reference — nothing is pinned |
| [0023](0023-a-heap-query-ships-and-reports-the-pause-it-imposed.md) | The heap query ships and reports the pause it imposed, rather than refusing or asking permission |
| [0024](0024-per-test-timings-come-from-libtest.md) | Per-test timings come from libtest's `--report-time` — so the runner builds and runs in two steps — and not from cargo-nextest, whose process-per-test scheduling is the one variable that must not move while the flakes are open |
| [0025](0025-the-suite-is-sharded-two-ways-by-measured-duration.md) | The suite is sharded two ways per JDK, split by *measured* duration rather than by name — and stops at two. Amended twice: the 70 s floor died with TEST-30 and the 35 s one with TEST-35, and with no floor left a third shard is measured at **0.1 s** for +50 % runners |
| [0026](0026-a-spent-stop-point-is-reported-spent-and-clearing-it-sends-nothing.md) | A stop point whose `hit_count` has fired is reported **SPENT** — a third state, because the *debuggee* removed it — and clearing it sends no packet, since JDWP request ids recur and a `Clear` for a deleted id can land on somebody else's |
| [0027](0027-an-instance-filter-is-offered-only-where-it-was-measured-to-apply.md) | `instance_id` is offered only on the stop-point kinds where `InstanceOnly` was **measured** to apply — HotSpot accepts it on three more and silently ignores it — and the arm reply says that an armed filter *pins* its object |

The downstream consumer's contract is [`docs/toolkit-contract.md`](../toolkit-contract.md) — what shipping
a change here costs the toolkit that packages it.

The root [`CONTEXT.md`](../../CONTEXT.md) now exists — the glossary reached the same point these did once
TRACE-5, DUMP-1 and METH-1 forced `stop point`, `snapshot`, and the `suspended` / `blocked` distinction to be
pinned down. It is a glossary only; decisions live here.
