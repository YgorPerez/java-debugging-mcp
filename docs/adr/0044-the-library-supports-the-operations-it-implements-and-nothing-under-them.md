# 0044 — The library supports the operations it implements, and nothing under them

## Context

`java-debugging-jdwp-client` has been on crates.io since v0.20.0 (REL-5, ADR-0043), published because
`cargo publish` rejects a bare path dependency and `jdwp-mcp` needs one. Its `lib.rs` answered the
"what is supported" question by declining it — *"This is not a supported public API … the crate is
published because `jdwp-mcp` depends on it, and for no other reason … anything here may change in any
release, including a patch one."*

CLEAN-2 (#170) made the cost of that visible. `cargo hawk check` reports **169 public items that nothing
outside the crate uses**, all of them in this library, on the exact surface `scripts/semver-check.sh` runs
its 196 checks against. Every one is something a release can break, and nobody had said which were design.

**The framing the issue used — "API or accident?" — is not the axis that decides them.** Sorted by *whose
fact each item is*:

| | count | |
|---|---:|---|
| `commands.rs` — command sets, command numbers, event kinds, step sizes and depths | 106 | **JDWP's.** Transcribed from the specification |
| `reader::value_tags` — the tag table | 17 | **JDWP's** |
| `types.rs` id aliases nothing references (`ArrayId`, `StringId`, `ThreadGroupId`, …) | 7 | **JDWP's** id space |
| `JdwpConnection`'s own surface, the wire structs, `Value`, `Location` | ~26 | **Ours** |
| `InFlight`, `CommandRequest`, `spawn_event_loop`, `EventLoopHandle` | ~13 | Plumbing |

**130 of 169 — 77% — are a transcription of a published specification.** Asking whether
`command_sets::CLASS_TYPE = 3` is "our API" is close to a category error: we did not design it, we cannot
break it, and it cannot rot. `cargo hawk` cannot see that distinction, because it measures reachability
from the binary and that lumps the specification in with the event loop's internals.

So the question that actually decides those 130 is narrower: **is sending a raw JDWP command a supported
use?** `send_command` is `pub`, takes an arbitrary `CommandPacket`, and `next_id` is `pub`, so today a
consumer can send a command this crate never implemented — and the constant table is the vocabulary that
call needs.

Two things the code said that `lib.rs` did not:

- **The published crate ships exactly one example, and it is the raw-command one.** Twelve of the thirteen
  examples live under `../examples/` and are deliberately excluded from the package;
  `jdwp-client/examples/probe_heap_queries.rs` is inside the package root. It imports four constant modules
  and calls `send_command`. The front page said nothing was supported while the only shipped example
  demonstrated the deepest coupling available.
- **`send_command` carries no `guard_mutation`.** ADR-0001's sentence is *"the MCP layer does not decide
  what counts as mutation; the wire does"*, and that holds for the nine primitives SAFE-12 (#171)
  enumerates — but not for the raw send beneath them.

## Decision

**The supported surface is the operations this library implements, and the types in their signatures.**
Everything beneath them — the specification transcription, the raw send, the event loop — is
`pub(crate)`.

A JDWP command this crate does not implement is a pull request, not a workaround.

Computed rather than chosen, by taking what `mcp-server` actually names plus the transitive closure of
those signatures:

**Supported.** `JdwpConnection` and its ~80 operations; `JdwpError`, `JdwpResult`;
`types::{Value, ValueData, Location, Variable}` and the id aliases that appear in a signature;
`events::{Event, EventKind, MonitorEvent}`, `EventSet`, `EventFilters`; `SuspendPolicy`, `MonitorKind`,
`WatchKind`, `extra::StepDepth`; `reftype::{FieldInfo, MethodInfo}`, `method::LineTable`,
`stackframe::VariableSlot`, `thread::Frame`, `vm::{ClassInfo, VmCapabilities}`.

**Internal.** `commands::*`, `reader::value_tags`, the seven unreferenced id aliases, `send_command`,
`next_id`, `read_independently`, `CommandPacket`, `ReplyPacket`, and the event-loop types.

Three edge cases the computation turned up, each decided here so the next reader does not re-derive it:

**The JDWP error codes stay public.** `protocol::ERR_ABSENT_INFORMATION` and `ERR_INVALID_OBJECT` are
transcription like everything in `commands.rs`, but they appear inside `JdwpError::JdwpErrorCode(u16, _)`,
which a caller must match on. A slice of the specification survives *because an error carries it*, and that
is the general rule rather than an exception for two constants.

**`MAX_READS_IN_FLIGHT` stays public; `read_independently` does not.** They look like a pair and are not.
`mcp-server` chunks its thread list by the constant so its suspension budget is checked every window — that
is real cross-crate use and the constant's own doc explains it. It never calls the method: the crate's wave
operations do, internally. **The constant's doc comment currently implies otherwise** and needs a word
changed; the caller needs the window *size*, not the primitive.

**`probe_heap_queries.rs` moves out of the published package**, to `../examples/` beside the other twelve.
It is a probe for our own investigation, not a demonstration of supported use, and leaving it shipped would
contradict this ADR from inside the crate.

## Consequences

**This is a breaking change to a published crate, and it is free exactly once.** `cargo-semver-checks`
treats `0.y.z`'s `y` as the major position, so `0.20.0 → 0.21.0` already permits it and
`scripts/semver-check.sh` will not block the tag. A `0.20.1` patch cut first would block it. It belongs in
the release notes as a breaking change even though, by the terms `lib.rs` published under, nobody was
entitled to rely on any of it.

**Narrow the items, not the modules.** Measured on this tree: `pub(crate) mod commands;` does not compile,
because the example above imports it cross-crate — and `clippy::redundant_pub_crate`, which DOC-13 records
reverting `method_name_matches` over, fires on a `pub(crate)` item inside a *private* module. Leaving every
module `pub` and narrowing the items avoids both. Verified at 0 findings.

**ADR-0001 needs a sentence, not a change of position.** With `send_command` internal, the read-only hole
is reachable only from inside this crate — but "the wire decides" is still imprecise about a guard that
sits on nine primitives rather than on the socket. That ADR is amended to say so rather than to promise
more; it already scopes read-only as a guard against accident and not a security boundary.

**`cargo hawk` becomes usable as a gate later, and deliberately is not one yet.** Adding a gate over 169
findings is how a gate gets ignored — the same call CI-5 (#150) made about zizmor's 71. Once the count is
low and the surviving surface is declared in hawk's config, a gate is a follow-up worth having.

### What it actually came to: 169 → 20 (applied 2026-08-07)

**Zero was never reachable, and the residue is the decision rather than the leftovers.** Every item on the
Internal list above is now `pub(crate)`; hawk still reports 20, in three groups, and each is something this
ADR chose:

| | count | why hawk still reports it |
|---|---:|---|
| `JdwpConnection`'s own operations — `set_field_watch`, `set_method_exit_request`, `set_step`, `set_invoke_timeout_ms`, `invoke_timeout_ms`, `is_read_only` | 6 | **Supported by the rule at the top of this ADR.** hawk measures reachability from the binary, and `mcp-server` happens not to call these six. "The server does not call it" is not "the library does not implement it" — the same distinction the 130 constants turned on, one level up |
| `reader::value_tags`, as a **module** | 1 | *Narrow the items, not the modules*, decided above. Every constant inside it is `pub(crate)`; hawk asks for the module too |
| `hawk::unnecessary_restricted_visibility` — `pub(crate)` items whose uses all sit in one module | 13 | **A different question from the one this ADR answers.** It decided the `pub` / `pub(crate)` boundary — what leaves the crate. Whether a crate-internal item could be module-private is intra-crate tidying with no caller-visible component |

So a future gate is `hawk::dead_public` and `hawk::unnecessary_public` with those seven declared, not a
bare zero. That is the follow-up, and it is smaller than it looked from 169.

**Three things the work turned up that the list above did not predict.**

*The `dead_code` warnings are the cost of keeping the table.* 34 transcription items became `pub(crate)`
and unused, so rustc reports them — and the gate fails on warnings. `commands.rs` carries one file-level
`#![allow(dead_code)]` with the argument written at it; the seven id aliases carry one each, because unlike
`commands.rs` the rest of `types.rs` is live code where *unused* still means something.

*One deletion was nearly a mistake.* `ReplyPacket::id` is never read in the lib build and looked like dead
plumbing — but three of `connection.rs`'s wave tests assert on it, with the message *"result N carries the
wrong reply"*. That is ADR-0038's entire claim. It stays, with `#[allow(dead_code)]` and the reason at the
field; `InFlight::id`, which really was unread, is gone.

*The published package shipped two probes, not one.* `probe_monitor_events.rs` sat beside
`probe_heap_queries.rs` inside the package root with no `[[example]]` entry, auto-discovered by cargo. Both
moved to `../examples/`; the reason given above for the one covers the other exactly.

## Rejected

**Curating a real public API.** The alternative reading of #170, and the one that would make the 130
constants deliberate: publish the client as a JDWP library, with raw-command access as its point. Rejected
because it commits to a promise nobody has asked for — a deprecation cycle, a compatibility policy, and
`semver-check.sh` blocking releases for reasons unrelated to the debugger. This project's product is the
MCP server; the library is how cargo lets us ship it.

**A documented escape hatch** — keeping `send_command` and the constants public under a weaker promise than
the operations get. Rejected for the reason this repo removed two workflows: two promises in one crate is a
distinction nobody reads, and the weaker one is exactly where a stranger would build.

**Deleting the 38 `dead_public` items** rather than narrowing them. Twenty-six are protocol constants and
seven are id aliases documenting the JDWP id space; "the binary does not send this command" is not "this is
not part of JDWP". Deletion buys a smaller file and loses a table.
