# Compared with the other Java debugging MCP servers

Two other projects solve the same problem, and one of them is this repository's **parent**: this is a
fork of [`navicore/jdwp-mcp`](https://github.com/navicore/jdwp-mcp), which is where the JDWP client,
the 13 original tool names and the architecture diagram in [`development.md`](development.md) come
from. The other is
[`d4n-sec/jdb-mcp`](https://github.com/d4n-sec/jdb-mcp), an independent implementation built on JDI
instead of the wire protocol. All three are MIT.

The rows below were read off each repository — **code, not just README** — on **29 Jul 2026** and
**re-verified on 5 Aug 2026**. Neither upstream had moved in between: `navicore/jdwp-mcp` has been
archived on GitHub since 28 Apr 2026, and `d4n-sec/jdb-mcp`'s last push was 30 Jan 2026. What *had* moved
was this column — the tool count was six releases stale — and one row was **wrong to `jdb-mcp`'s
disadvantage**, undercounting their tools by half. Both are corrected below. A comparison a project writes
about its own competitors earns nothing by flattering itself, and the error was in the direction that
does. Re-check before quoting this.

| | **this fork** | `navicore/jdwp-mcp` | `d4n-sec/jdb-mcp` |
| --- | --- | --- | --- |
| Talks to the JVM through | JDWP, implemented natively (Rust) | JDWP, implemented natively (Rust) | JDI, the JDK's own debug API (Java) |
| Needs a JDK to run *the debugger* | no — one self-contained binary | no | **yes**, JDK 17+ (a JDK 7 build exists for legacy targets) |
| Debug tools | 38 | 13 | 24 (15 named in their README; counted from `McpTools.java`) |
| Stepping | yes | tool exists; the handler returns `"Step over not yet implemented"` | yes |
| Expression evaluation | chains, static and instance methods, overload resolution on runtime types, subscripts, deep expansion | same stub as stepping | not yet — `debug_calc` is the second item on its TODO list |
| Watchpoints / method-exit values | field read+write with old → new; return value **and which `return`** | no | `debug_set_watchpoint`, method entry/exit monitors |
| Conditional, hit-count and thread-filtered stop points | yes | no | on the TODO list |
| Wildcard / batched class patterns when arming | **yes** — one wildcard arms every matching loaded class *and* keeps arming ones that load later, addressable as one id; every arming tool also takes a list of patterns | no | on the TODO list (as "package prefix filtering" and "batch class filtering") |
| Non-suspending trace mode | yes, on **all five** arming tools — line, exception, field, method-exit and monitor stops — with measured per-hit cost; the default on method-exit and monitor stops | no | not documented |
| Hot reload / frame rewind | `RedefineClasses` + `PopFrames`, with all twelve HotSpot refusals mapped | no | no |
| Concurrent sessions | yes, with `debug.list_sessions` | single | single — explicitly, "eliminating the need for complex `sessionId` management" |
| Transports | stdio | stdio | stdio **and HTTP** |
| A hit reaches the agent by | `notifications/message` **push**, plus the buffered `debug.get_last_event` — push for *suspending* hits only, since a traced stop point is built to fire hundreds of times a second (EVT-2) | event loop is listed as a next step | **MCP notifications**, on by default and switchable with `--notifications`, plus a buffered `debug_get_events` — the same two-route shape as this column |
| Launching the target for you | **yes** — `debug.launch`, and it holds the JVM *before its first instruction* (`suspend=y`), so a breakpoint in a static initialiser can fire | no | no (`debug_launch` is on its TODO list) |
| `METHOD_ENTRY` monitors across a class pattern | **no**, deliberately (METH-1) | no | `debug_set_method_entry` |
| Maintenance | this repo | GitHub copy **archived 28 Apr 2026**, moved to `git.navicore.tech` (last commit there 6 Oct 2025, 22 commits total) | not archived; last push **30 Jan 2026** (v1.1.0), so six months quiet as of this re-check |

**What the fork actually changed.** Upstream's README still lists "event loop for async breakpoint
notifications", "stepping commands", "expression evaluation", "string and object dereferencing" and "full
MCP server integration" under *Next Steps*, and its `handle_evaluate` / `handle_step_*` return their TODO
strings — so upstream is a
working JDWP client and a set of tool names, and every capability in [`tools.md`](tools.md) except
attach, line breakpoints and `get_stack` was written here. The largest additions are the ones a shared,
long-running app server forces on you: an expression evaluator that resolves overloads the way `javac`
would, non-suspending trace mode, the watchdog and read-only mode, hot reload with `pop_frame`,
staleness detection, and a thread dump that names lock owners.

**Where the others are ahead.** `jdb-mcp` ships an **HTTP transport**; stdio is the only way into this
server. Counted properly it also has a **larger tool surface than this file used to credit it with** — 24
against the 13 once claimed here — and three of those have no counterpart in this column.
`debug_get_output` and `debug_send_input` read and write the debuggee's own stdio: unfiled here rather
than unconsidered, and the reason changed once `debug.launch` existed. The original objection was that an
attach-only connection has no process handle; it now has one for a launched JVM, so what keeps them
unfiled is narrower — the diagnostic half of what they are for, a dead debuggee's last words, is already
in the `launch`, `disconnect` and `list_sessions` replies, and the interactive half is not something a
shared app server has. The third is `debug_set_method_entry`, a `METHOD_ENTRY` monitor across a class
pattern, which here is a deliberate omission rather than a gap — with a `ClassMatch` it is the noisiest event in JDWP,
and "what called this?" is answered far more cheaply by a traced breakpoint's caller chain (METH-1,
TRACE-5) — but if you want that firehose, it has one and this does not. Being JDI-based it also
inherits Oracle's implementation of the hard parts: a wire-protocol bug here is ours to find, and some
have been (the packet reader had to become its own task because `select!` was cancelling it mid-packet).
Upstream, for its part, is a much smaller codebase and still the clearest place to read how JDWP framing
works in Rust.

**Their roadmaps, measured against this.** Upstream's five *Next Steps* — event loop, stepping,
expression evaluation, string/object dereferencing, full MCP server integration — are all implemented here. `jdb-mcp`'s six TODO items
are now **all** implemented here: expression evaluation, multi-session, and conditional + thread-filtered
breakpoints already were; package-prefix filtering, batch class filtering and `debug_launch` were built on
2026-07-29 (FILT-3, FILT-4, LAUNCH-1) *because* this comparison surfaced them. That is not a claim about
`jdb-mcp` — a TODO list is a statement of intent, and theirs may well arrive with a better shape than ours.

**Where the three differ on starting the JVM.** The upstreams are attach-only: you start the JVM with
`-agentlib:jdwp` yourself. This fork does both, and the difference is not convenience — attaching can never
break on code that runs *during* startup, because by the time a connection is possible the static
initialisers have run. `debug.launch` holds the VM before its first instruction. A launched JVM also has no
one else on it, so the shared-instance cautions that shape everything else here do not apply to it.

**Where none of the three differ.** None is a security boundary — anyone who can reach a JDWP port owns the
JVM.
