# Compared with the other Java debugging MCP servers

Two other projects solve the same problem, and one of them is this repository's **parent**: this is a
fork of [`navicore/jdwp-mcp`](https://github.com/navicore/jdwp-mcp), which is where the JDWP client,
the 13 original tool names and the architecture diagram in [`development.md`](development.md) come
from. The other is
[`d4n-sec/jdb-mcp`](https://github.com/d4n-sec/jdb-mcp), an independent implementation built on JDI
instead of the wire protocol. All three are MIT.

A fourth server is compared at the bottom of this file rather than in the table:
[`kpanuragh/xdebug-mcp`](https://github.com/kpanuragh/xdebug-mcp) debugs PHP, so the rows below — which are
mostly about JDWP mechanics — would be empty for it. It gets a section because its tool surface is already
cited in three ADRs here, and reading them one at a time cost a comparison that had already been done
(DOC-16, #161).

The rows below were read off each repository — **code, not just README** — on **29 Jul 2026** and
**re-verified on 5 Aug 2026**. Neither upstream had moved in between: `navicore/jdwp-mcp` has been
archived on GitHub since 28 Apr 2026, and `d4n-sec/jdb-mcp`'s last push was 30 Jan 2026. What *had* moved
was this column — the tool count was six releases stale — and one row was **wrong to `jdb-mcp`'s
disadvantage**, undercounting their tools by half. Both are corrected below. A comparison a project writes
about its own competitors earns nothing by flattering itself, and the error was in the direction that
does. Re-check before quoting this. This column's count moved again on **7 Aug 2026**, from 38 to 40; the
other two were not re-read that day and keep their 5 Aug date.

| | **this fork** | `navicore/jdwp-mcp` | `d4n-sec/jdb-mcp` |
| --- | --- | --- | --- |
| Talks to the JVM through | JDWP, implemented natively (Rust) | JDWP, implemented natively (Rust) | JDI, the JDK's own debug API (Java) |
| Needs a JDK to run *the debugger* | no — one self-contained binary | no | **yes**, JDK 17+ (a JDK 7 build exists for legacy targets) |
| Debug tools | 40 (38 on 5 Aug; re-counted 7 Aug) | 13 | 24 (15 named in their README; counted from `McpTools.java`) |
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

## A fourth surface, in another language: `kpanuragh/xdebug-mcp`

[`kpanuragh/xdebug-mcp`](https://github.com/kpanuragh/xdebug-mcp) is not a Java server and is not on the
list above. It is a TypeScript MCP server that speaks **DBGp** to Xdebug, so the language it debugs is PHP.
It is here because its tool surface is already load-bearing in this repository's decisions and has been for
weeks: ADR-0040 weighs its watch family, ADR-0041 its debug profiles, ADR-0042 its `export_session`.

Reading those one ADR at a time is how the whole comparison got **re-derived from scratch on 7 Aug 2026** —
and three of the five gaps that exercise produced were rejections this repo had already written down and
argued. This section exists so that does not happen a second time (DOC-16, #161). The rule it is trying to
enforce is the one [`toolkit-contract.md`](toolkit-contract.md) keeps pointed the other way — *tools the
downstream pin has that the docs never name*. Turned around, it reads: a decision this repo has taken and
no comparison names is a decision that will be taken again.

**Read off `src/tools/*.ts` at tag `v1.3.0` on 7 Aug 2026** — the `server.tool(…)` registrations rather than
the README, the same standard as the table above. **46 tools**, against **40** in this column at 0.20.0. MIT,
not archived, last push 5 Aug 2026. Every one of the 46 is in exactly one table below; the counts are stated
so that claim is checkable rather than asserted.

**One structural difference explains most of the delta, and it is not about either project's ambition.** A
PHP debug session is one HTTP request. It begins when Xdebug connects and it is gone in milliseconds,
whether or not anyone was finished with it. A JVM debug session outlives an afternoon, on a server other
people are also using. So almost everything xdebug-mcp has and this does not is machinery for surviving a
debuggee that dies every few seconds — breakpoints that can be set before any session exists, saved debug
profiles, captured request context, a logpoint history that outlives the run. And almost everything this has
and it does not is machinery for **not disturbing a debuggee that will still be there tomorrow**:
non-suspending traces with a measured per-hit cost, hit budgets, the watchdog, staleness verdicts,
read-only mode. Neither list is a gap in the other project.

The arrow points the other way in exactly one place, and it is the reason #157 exists: holding several
long-lived JVMs at once makes *which session am I talking to* a real question, and here it is not a tool.

### Has a counterpart here — 27 of 46

| xdebug-mcp | here |
| --- | --- |
| `set_breakpoint` | `debug.set_line_stop`, which also takes `condition`, a hit count and a thread filter |
| `set_exception_breakpoint` | `debug.set_exception_stop` |
| `set_call_breakpoint` | `debug.set_line_stop {method}` |
| `remove_breakpoint` | `debug.clear_stop_point` |
| `list_breakpoints` | `debug.list_stop_points` |
| `continue` | `debug.continue` |
| `step_into` / `step_over` / `step_out` | `debug.step_into` / `debug.step_over` / `debug.step_out` |
| `detach` | `debug.disconnect` |
| `get_stack_trace` | `debug.get_stack` |
| `get_variables` | `debug.get_stack {include_variables: true}` |
| `get_variable` | `debug.evaluate`, or `debug.evaluate_chain` for a path through several hops |
| `set_variable` | `debug.set_value` |
| `evaluate` | `debug.evaluate` |
| `get_source` | `debug.source` — which also answers the question PHP does not raise: *which file was this class compiled from*, read from the JVM rather than from disk |
| `list_sessions` | `debug.list_sessions` |
| `get_session_state` | `debug.list_sessions` for status, holds and counts, plus `debug.get_stack` for the position |
| `close_session` | `debug.disconnect` |
| `add_logpoint` / `remove_logpoint` | `trace: true` with `trace_expr`, on any of the five arming tools — cleared with `debug.clear_stop_point`, silenced without losing its expression by `debug.toggle_stop_point` |
| `get_logpoint_history` | `debug.get_traces` |
| `save_debug_profile` / `list_debug_profiles` | `debug.list_stop_points {export: true}` (ADR-0041) |
| `load_debug_profile` | `debug.arm_stop_points` (ADR-0041) |
| `export_session` / `capture_snapshot` | `debug.export_investigation` (ADR-0042) |

### Settled against, with the file that settled it — 12 of 46

**This is the table that stops the re-derivation.** Each row is a decision with an argument and a rejected
alternative already written down; none of them is an unbuilt feature.

| xdebug-mcp | settled by |
| --- | --- |
| `add_watch` / `remove_watch` / `evaluate_watches` / `list_watches` | [ADR-0040](adr/0040-a-session-default-not-a-second-tool-family-and-never-a-merge.md) — rejected on four counts in favour of a session default. The three rules that came out of it (never a merge, clamped once at attach, every reply says when it took the default) are what a new default here has to argue against |
| `start_profiling` / `stop_profiling` / `get_profile_stats` / `get_memory_timeline` | [`.out-of-scope/profiling-and-coverage.md`](../.out-of-scope/profiling-and-coverage.md) (PROF-1, #140) — JDWP has no sampling and no allocation profiling; this is a gap in the wire protocol, not in the implementation. Reaching them means JFR, JVMTI or a bytecode-instrumenting agent, and the last two mean modifying the running application to measure it |
| `start_coverage` / `stop_coverage` / `get_coverage_report` | the same file — JDWP has no coverage command set either, and the same no-agent promise applies |
| `get_function_history` | [`.out-of-scope/method-entry-events.md`](../.out-of-scope/method-entry-events.md) (METH-1) — `METHOD_ENTRY` filters by *class*, not by method, so it is the noisiest event in JDWP. *What called this?* is answered by a traced stop point's caller chain instead (TRACE-5), at one site rather than every method on the class |

### Open here, each with its issue — 5 of 46

Each of these came out of the 7 Aug 2026 comparison and is filed rather than settled.

| xdebug-mcp | issue |
| --- | --- |
| `set_active_session` | **#157** (SESS-1) — the current session cannot be changed, so a second attach is the only way back to a JVM already held |
| `add_step_filter` / `list_step_filters` | **#158** (STEP-2) — the step filter is the one session preference that must be restated on every call |
| `update_breakpoint` | **#159** (BP-9) — narrowing a condition means clearing the stop point, which discards its id and its hit tally |
| `capture_request_context` | **#160** (DISC-15) — *what did this request carry?* costs six invoking evaluates on a thread you had to suspend |

### No counterpart, deliberately — 2 of 46

| xdebug-mcp | why not |
| --- | --- |
| `get_contexts` | PHP's contexts are Local, Superglobals and user-defined constants. Java has no superglobals: `debug.get_stack {include_variables: true}` returns the locals, and a static field is reached by name through `debug.evaluate` |
| `stop` — terminate the script immediately | The one thing this server is built not to do. Every design decision here starts from a JVM that is shared and outlives the session, which is why `debug.disconnect` detaches and leaves it running and `debug.panic` puts it *back* rather than ending it. On a PHP request that was going to die in milliseconds anyway, the same tool costs nothing |

**Where xdebug-mcp is ahead: nothing found on debugging capability, as of 7 Aug 2026.** Said explicitly
rather than left as an omission, because an omission is how this file was once wrong to `jdb-mcp`'s
disadvantage. Every one of its 46 tools is above in one of four tables, and none of the four is "they have
this and we do not, unexamined".

Two things it is ahead on that are not capability, and both are already on the record here: it **publishes
a documentation site** and this repo does not ([`.out-of-scope/published-documentation-site.md`](../.out-of-scope/published-documentation-site.md),
which was written while comparing against exactly this project), and DBGp gives it a **proxy registration
story** for shared hosts that has no analogue here ([`.out-of-scope/http-transport.md`](../.out-of-scope/http-transport.md)).
Re-check before quoting any of this.
