# jdwp-mcp

**Java debugging for LLMs via JDWP and Model Context Protocol**

An MCP server that enables Claude Code and other LLM tools to debug Java
applications through the Java Debug Wire Protocol (JDWP). Attach to running
JVMs, set breakpoints, inspect variables, and step through code—all through
natural language.

## Features

- **Remote Debugging**: Connect to any JVM started with JDWP enabled
- **Breakpoint Management**: Set/list/clear by class+line — with optional **hit-count** (stop on the
  Nth hit) and **thread filters**, or set by **method name** (first line)
- **Stack Inspection**: Stack frames with typed local variables and resolved **source lines**
- **Execution Control**: **Step over/into/out**, continue, pause
- **Expression Evaluation**: `localVar`/`this`/`Class` heads with `.field` and `.method(args)` chains
  — including **static fields and static methods** (`ConfigDefaultUtils.getUrl()`) — resolving
  overloads by the arguments' **runtime types**, including interfaces they implement (walked
  transitively) and autoboxing, and refusing an argument a parameter can't accept. Arguments are
  literals (int, long, boolean, null, `"string"`) **or expressions passed by reference**
  (`svc.matches(reserva)`, `foo.handle(this, cfg.getId())`)
- **Value Rendering**: Strings, typed objects (best-effort `toString()`), and **array contents**
- **Recursive Expansion**: `expand_objects:true` on `debug.evaluate` / `debug.get_stack` walks nested
  objects, arrays, and **`List`/`Set`/`Map`/`Optional` contents** into a field tree — bounded by
  `max_depth`/`max_children` and a node budget, with **cycle detection** and unboxed wrappers
- **Collection Subscripts**: `lines[0]`, `counts["key"]`, `lines[2..5]` (slice) and
  **`lines[?qty > 3]`** (filter, with the left side resolved against each element). Filtering a `Map`
  keeps the keys (`key → value`), and a single element can be **written** as well as read
- **Set Values**: a local, a static or instance field, or one element of an array / `List` / `Map`
- **Field Watchpoints**: break when a field is read or written — `debug.set_field_stop` reports the
  mutating location with the **old → new** value, for "who changes this behind my back?"
- **Method return values**: `debug.set_method_exit_stop` reports **what a method returned and from
  which `return`**, so a method with several exits (or one whose value comes from a chain you can't
  break on) stops being a guessing game. Trace mode is the default for this one
- **Non-suspending trace mode**: `trace:true` on a breakpoint, an **exception breakpoint** or a
  **watchpoint** snapshots the hit and resumes the thread immediately instead of freezing the VM —
  the only safe way to use any of them on a shared instance. Each snapshot carries the **calling
  chain** above the hit (`trace_frames`, default 3), so a logpoint answers *which path reached this*.
  Read the snapshots with `debug.get_traces`.
  It does **not freeze** the VM, which is not the same as not **slowing** it: capture is serialised
  through one connection, so a traced stop point tops out at ~**720 hits/s** (~1160 at
  `trace_frames: 0`) and hits past that queue. Under a few hundred hits/s that is effectively free, and
  `trace_max_hits` (default 200) keeps even a hot site to a sub-second blip — `trace_max_hits: 0` is
  the one setting that removes that bound, and the arm reply warns when you use it
  *(loopback figures against a trivial endpoint; the ceiling is the durable part, not the percentage)*
  You do not have to take those figures on trust: once a traced stop point has fired,
  `debug.list_stop_points` reports what **it** is costing on **your** JVM — the mean capture per hit, the
  rate it could sustain before hits queue, and the rate hits are arriving at with the share of the window
  spent capturing. A traced stop point that has captured nothing says so, rather than reporting zero
- **Thread Management**: tools default to the last thread that hit a breakpoint
- **Thread dumps with lock ownership**: `debug.thread_dump` answers "it's wedged — which threads are
  blocked on what?" in one call: every thread's stack, the monitors it holds, the one it is blocked
  entering, and **who holds that** — so a deadlock cycle is visible without leaving the debugger
- **Structured Events**: `get_last_event` emits a machine-readable `[event]` line (thread, class.method:line),
  from a bounded buffer — a burst of hits isn't lost, and the reply says how many are still pending
- **Safety**: a `panic` tool (clear all + resume) and a **watchdog** that auto-resumes a long-suspended
  VM (`JDWP_WATCHDOG_SECS`, default 120) so a forgotten breakpoint can't freeze a shared instance

> This fork implements `debug.evaluate` and `debug.step_*` (stubs upstream) plus the safety,
> structured-event, array, set-value, and breakpoint-modifier features above.

## Quick Start

### 1. Start your Java app with JDWP enabled

```bash
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005 -jar myapp.jar
```

### 2. Build the MCP server

```bash
cargo build --release
```

### 3. Configure Claude Code

The easiest way to enable the MCP server for your project:

```bash
# From your Java project directory
claude mcp add --scope project jdwp /path/to/jdwp-mcp/target/release/jdwp-mcp
```

Adjust the path to match where you cloned this repository. The `--scope project` flag makes the debugger available only in your current Java project.

**Alternative**: Manual configuration via `.mcp.json`:

```json
{
  "mcpServers": {
    "jdwp": {
      "command": "/path/to/jdwp-mcp/target/release/jdwp-mcp"
    }
  }
}
```

### 4. Debug with natural language

```
> Attach to the JVM at localhost:5005
> Set a breakpoint at com.example.HelloController line 65
> When it hits, show me the stack and the value of requestCount
```

## Available Tools

| Tool | Description |
|------|-------------|
| `debug.attach` | Connect to a JVM via JDWP |
| `debug.set_line_stop` | Set a breakpoint by class+line, or by method name; optional `hit_count`, thread filter, `condition` (with `&&`/`||`), or `trace:true` (non-suspending logpoint, with `trace_max_hits` and `trace_frames`) |
| `debug.set_exception_stop` | Break when an exception (of a class + its subclasses, or all) is thrown; `caught`/`uncaught` selectable, an optional `thread_id` filter, or `trace:true` (with `trace_max_hits` / `trace_frames`) to collect throws without suspending |
| `debug.get_traces` | Read snapshots captured by any `trace:true` stop point — line, exception or watchpoint, each with the caller chain above it (bounded ring buffer; narrow with `bp_id` / `class_filter` / `since`, optional `clear`) |
| `debug.list_stop_points` | List active stop points (line, deferred, exception, watchpoint, method-exit) with trace budgets and thread filters — plus, for each traced one, its **measured** capture cost: mean per hit, the rate it can sustain, and the rate hits are arriving at |
| `debug.clear_stop_point` | Remove a stop point (line, deferred, exception, watchpoint, or method-exit) |
| `debug.toggle_stop_point` | Silence or re-arm any stop point (`bp_…` / `exc_…` / `watch_…` / `mexit_…`) without losing its `condition`/`trace_expr`; the id stays the same across the round trip |
| `debug.continue` | Resume execution |
| `debug.step_over` | Step over current line (defaults to last-hit thread) |
| `debug.step_into` | Step into a method call |
| `debug.step_out` | Step out of the current method |
| `debug.get_stack` | Stack frames, compact `#i class.method:line` with typed locals indented |
| `debug.evaluate` | Evaluate `var`/`this`/`Class` + `.field` / `.method(args)` chains in a frame; static methods, object arguments, `[i]`/`["k"]`/`[a..b]`/`[?pred]` subscripts (predicates support `&&`/`||`), and `expand_objects` for a deep field tree |
| `debug.set_value` | Write a local variable, a static field (`Class.field`), an instance field (`this.field`), or one element (`xs[0]`, `counts["k"]`) — from a literal or a copied live reference (`this.a = other.b`) |
| `debug.set_field_stop` | Break when a field is written (or read) — reports the mutating location + old → new value; optional `thread_id` filter; `trace:true` (with `trace_max_hits` / `trace_frames`) collects hits without suspending |
| `debug.set_method_exit_stop` | Report what a method **returned**, and from which `return` — for a method with several exits, or a value from a chain you can't break on. `class_pattern` + `method`; `trace` defaults to **true** here (a suspending method exit on a hot method freezes a VM fastest), and a broad suspending request is refused with the reason |
| `debug.force_return` | Force the current method to return a given value, skipping the rest of its body |
| `debug.get_last_event` | Last event as a machine-readable `[event]` line (thread, class.method:line, exception type, watched field's old → new) + `[suspended]`; events are buffered, so `limit` reads a backlog and `drain` discards it |
| `debug.list_threads` | List threads by name; filter with `name_filter` / `only_suspended` / `limit` |
| `debug.thread_dump` | Every thread's stack in one call **plus** the monitors each holds and the one it is blocked entering, with the blocker named (`← held by 0x<id> "<name>"`) — a deadlock cycle is readable straight off it. JDWP can only read a *suspended* thread, so pass `suspend:true` (freeze, read, resume, verify) or `only_suspended:true`; it never suspends on its own. Bound the cost with `name_filter` / `limit` / `max_frames` / `package_filter`, and the freeze with `max_suspend_ms` (default 2000) — the reply reports how long it held the VM, the packets it spent, and any threads a budget made it skip. `monitors_only:true` reads the lock graph without the stacks for a fraction of the freeze (measured: 245 packets / 33 ms against 770 / 117 ms on a 60-thread dump) |
| `debug.pause` | Pause execution (suspend all threads) — watchdog-covered, so a forgotten pause can't freeze the JVM |
| `debug.panic` | Safety: clear all stop points and resume all threads |
| `debug.list_sessions` | List live sessions — `host:port`, which is current, suspended or DEAD, and how many stop points/traces/events each holds |
| `debug.disconnect` | End the debug session |

> **Renamed in VOCAB-1 (#20).** The stop-point tools were called `set_breakpoint`,
> `set_exception_breakpoint`, `set_watchpoint`, `set_method_breakpoint`, `clear_breakpoint`,
> `list_breakpoints` and `toggle_breakpoint`. "Breakpoint" named three different scopes across them —
> one source location, two things that were not locations, and all four kinds — while `set_watchpoint`
> was a stop point the word did not cover at all. The names above follow `CONTEXT.md`, where **stop
> point** is the umbrella and **breakpoint** means a line breakpoint. Arguments are unchanged —
> `breakpoint_id` on clear/toggle, `bp_id` on `get_traces`, and the `bp_…` / `exc_…` / `watch_…` /
> `mexit_…` id prefixes all keep their names.

Most tools take `thread_id` as an optional hex string (e.g. `"0x2"`); when omitted they default to
the last thread that hit a breakpoint.

**Keeping a shared JVM safe.** A watchdog auto-resumes a VM left suspended for too long
(`JDWP_WATCHDOG_SECS`, default 120; `0` disables) — whether a stop point or a `debug.pause` froze it —
and *disables* whatever caused it, so it can't re-freeze on the next hit. JDWP **counts** suspends, so
resuming is treated as "make it actually run", not one Resume packet: `continue`, `panic` and the
watchdog clear the whole suspend depth and verify it via `SuspendCount` before reporting success, and
`debug.pause` is idempotent so a depth can't build up by accident. Disabling keeps the
definition, so one `debug.toggle_stop_point` re-arms it with its condition and `trace_expr` intact;
the same applies when a `trace:true` stop point hits its `trace_max_hits` budget.
`debug.disconnect` resumes the VM and clears every request on the way out, so it can never leave a
shared JVM frozen.

**Read-only sessions.** Set `JDWP_READONLY=1` (or `read_only:true` on `debug.attach`) to refuse
everything that would execute code in the target: method invocation, writes, and `force_return`.
Enforced on the connection itself rather than by inspecting expressions, so the indirect paths are
covered too — `toString()` rendering, `List`/`Map` subscripts, and breakpoint `condition`/`trace_expr`
(refused when you arm them, not silently on each hit). The honest cost is shallower output: objects
render as `Type (id=0x…)`, because pretty-printing one means invoking it. Reads that need no
invocation are unaffected — locals, fields, statics, array indexing, `get_stack`, and
watchpoint/exception reporting. A guard against accidentally mutating a production JVM, **not** a
security boundary: anyone who can reach the JDWP port can open their own connection without it.

## Example: Debugging with kubectl port-forward

For Kubernetes-deployed Java apps:

```bash
# Forward JDWP port from pod
kubectl port-forward pod/my-app-pod 5005:5005
```

Then in Claude Code:
```
> Attach to localhost:5005
> Set a breakpoint in the processRequest method
```

## Architecture

```
Claude Code → MCP Server → JDWP Client → TCP Socket → JVM
                ↓
         Summarization &
         Context Filtering
```

The MCP server handles:
- **Protocol Translation**: MCP JSON-RPC ↔ JDWP binary protocol
- **Smart Summarization**: Truncates large objects, limits depth
- **State Management**: Tracks breakpoints, threads, sessions

## Development

### Project Structure

```
jdwp-mcp/
├── jdwp-client/        # JDWP protocol implementation
│   ├── connection.rs   # TCP + handshake
│   ├── protocol.rs     # Packet encoding/decoding
│   ├── commands.rs     # JDWP command constants
│   ├── types.rs        # JDWP type definitions
│   └── events.rs       # Event handling
├── mcp-server/         # MCP server
│   ├── main.rs         # Stdio transport
│   ├── protocol.rs     # MCP JSON-RPC
│   ├── handlers.rs     # Request routing
│   ├── tools.rs        # Tool definitions
│   ├── session.rs      # Debug session state
│   └── tests/          # MCP-level integration tests (real binary + real JVM)
└── examples/
    ├── test_*.rs       # jdwp-client protocol examples
    └── probes/         # Java programs the tests and examples attach to
```

### Testing

```bash
cargo test                      # unit tests + the stdio protocol tests (fast, no JVM)
scripts/integration-test.sh     # MCP-level: the real binary over JSON-RPC against probe JVMs
scripts/doctor.sh               # the rust-doctor health gate CI runs
```

`scripts/integration-test.sh` runs `mcp-server/tests/mcp_integration.rs`, which launches and reaps its
own probe JVMs from `examples/probes/` — no manual steps. It does need a JDK: without one every test
prints `SKIP` and passes, so check for `SKIP` lines before reading a green run as coverage.

`mcp-server/tests/stdio_protocol.rs` is the exception: it drives the real binary's JSON-RPC front door
with malformed input (unparseable lines, non-objects, missing `method`, EOF mid-message) and needs no JDK,
so it runs in plain `cargo test`. Each case checks that an error came back **and** that the server is
still serving afterwards, since one bad line from a client must not end the session.

#### Testing shared-instance behaviour without a shared instance

The costs that matter on a busy remote JVM — how long a dump freezes it, how much a trace slows it — used
to be answerable only against the real thing. They aren't. Three variables separate a real app server from
a loopback probe, and two of them belong to the debuggee:

| variable | how a test presents it |
| --- | --- |
| hundreds of threads, not tens | `PoolShapeProbe` — 300 workers, named like a real pool |
| stacks far deeper than `max_frames` | `PoolShapeProbe` — 60 **distinct** frames per worker |
| a network hop instead of loopback | `LatencyRelay::start(probe.port, rtt)`, then attach to `relay.port` |

`LatencyRelay` forwards the JDWP stream adding a measured round trip, in userspace — `tc … netem` needs
`NET_ADMIN`, and deterministic latency beats a real network's jitter for a test. It charges coalesced
traffic once, so measurements through it are a lower bound, and it models latency only.

The cost model these established is `held ≈ packets × (our per-packet cost + RTT)`, measured linear in RTT
with a slope of 1 packet per round trip. So **packet count is the lever**, which is why a dump caches line
tables per call (ADR-0011) rather than being given a longer suspension budget. Assert packet counts, not
durations: a packet count is deterministic and load-independent.

You do not have to take any of these figures on trust against your own instance, either: a dump reports
what **it** cost there —

```
🧵 Thread dump — 40/306 thread(s)
   ⏱  Held the VM suspended for 779ms.
Cost: 258 JDWP packet(s), 3.08ms each (round trip + our own processing).
```

— and a dump the budget truncated says what finishing would have taken at the rate it was running, so the
choice between narrowing it and raising `max_suspend_ms` is made against a number rather than a guess.
Measured with the relay, the defaults hold the VM inside the 2000 ms budget up to roughly a **6 ms round
trip**; past ~7 ms even a defaults dump truncates, which is the safety net working.

For poking at the tools by hand against a realistic app, use the companion
[java-example-for-k8s](../java-example-for-k8s) as a target:

```bash
cd ../java-example-for-k8s
mvn clean package
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005 \
  -jar target/probe-demo-0.0.1-SNAPSHOT.jar
```

### Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test
```

### Code health

[rust-doctor](https://github.com/arthjean/rust-doctor) folds clippy, `cargo-audit`/`deny`/`geiger`,
and custom AST rules into one 0–100 score. Run it locally (no Rust build of the tool — `npx` fetches
a prebuilt binary):

```bash
scripts/doctor.sh              # score card for the workspace
scripts/doctor.sh --verbose    # per-finding file:line detail
scripts/doctor.sh --diff main  # only files changed vs main
```

The same check runs in CI (`.github/workflows/rust-doctor.yml`, pinned to 0.2.0): it gates on errors
and uploads results to GitHub code scanning (SARIF). Installing the optional external tools
(`cargo install cargo-audit cargo-deny cargo-machete cargo-geiger`) unlocks the dependency/unsafe
passes it otherwise skips.

### Serena (semantic code navigation for agents)

[Serena](https://github.com/oraios/serena) is registered as an MCP server for this repo, giving an agent
symbol-level navigation over the Rust workspace instead of grep-and-read. The repo carries the shared
configuration; each machine needs a one-time install.

**One-time setup:**

```bash
# uv (Serena is a Python tool), then Serena itself
winget install astral-sh.uv            # or: curl -LsSf https://astral.sh/uv/install.sh | sh
uv tool install -p 3.13 serena-agent
serena init

# Rust support uses rust-analyzer from your rustup toolchain
rustup component add rust-analyzer

# Build the symbol cache once (a few seconds; it is gitignored)
serena project index .
```

Committed here, so nothing else is needed: `.mcp.json` (the server registration, using
`--project-from-cwd` so it contains no absolute paths), `.serena/project.yml` (Rust only — the Java files
under `examples/probes/` are fixtures and get no language server), and `.claude/settings.json`
(Serena's hooks).

**One thing worth knowing before you rely on it**, measured on this workspace by tracing the LSP traffic:

**Semantic queries return empty for the first ~2.5 minutes of a session, then work correctly.**

rust-analyzer signals `quiescent` after about **152s** here — it spends that time on `Fetching`,
`Building compile-time-deps`, `Building CrateGraph` and `Loading proc-macros` for the dependency tree.
Serena stops waiting at a **hard-coded 120s** (`_SERVER_READY_TIMEOUT` in its `rust_analyzer.py`) and
proceeds anyway, so a query in that ~30s gap is sent to a server that is not ready: rust-analyzer answers
`[]` and the tool reports `{}`.

What that means in practice:

| | behaviour |
| --- | --- |
| `find_symbol`, `get_symbols_overview` | work immediately — document symbols only need parsing |
| `find_referencing_symbols` and other semantic queries | empty before ~152s, **correct after** |
| after quiescence | ~30ms–3s per query, including cross-crate references |

**Raising the wait fixes it**, and is worth doing: at the default the first semantic query burns two
minutes *and* returns an empty result, whereas with a longer wait it takes ~152s and is correct.

The limit is a hard-coded local in Serena (`_SERVER_READY_TIMEOUT = 120.0` in
`solidlsp/language_servers/rust_analyzer.py`) with no env var or config key, so it takes a one-line patch
to make it configurable:

```bash
scripts/serena-ready-timeout.sh            # apply (rewrites the constant to read an env var)
scripts/serena-ready-timeout.sh --check    # report status; exit 1 if not applied
scripts/serena-ready-timeout.sh --revert   # restore the original line
```

It keeps `120` as the default and reads `SERENA_RUST_READY_TIMEOUT`, which `.mcp.json` sets to `300` for
this repo. It is idempotent and refuses to run if the upstream line has changed — **re-run it after
`uv tool upgrade serena-agent`**, which replaces the file. `--check` is a useful thing to run if semantic
queries start coming back empty again.

Without the patch, nothing is broken; just re-run a query that came back empty.

Two other setup notes:

- **`export MCP_TIMEOUT=300000`.** Serena's docs suggest `60000`; that is not enough here.
- **Don't conclude "no references" from an early empty result.** That is the one genuinely misleading
  behaviour, and it is a timing artefact rather than a limitation.

Tuning rust-analyzer instead was measured and does not help: disabling `cachePriming` and `check` saves
only ~5s of the 152s, and the settings that *would* help (`procMacro.enable: false`,
`buildScripts.enable: false`) would break analysis of the derive macros this codebase is full of.

Serena's own docs note that Claude Code's built-in tool descriptions bias the model strongly toward
internal tools. The committed hooks are their recommended mitigation; they also suggest launching with

```bash
claude --system-prompt="$(serena prompts print-cc-system-prompt-override)"
```

which is left to you, since it changes how you start Claude Code rather than anything in this repo.

Serena's **memories are deliberately not versioned** (see `.gitignore`). This repo keeps its curated
knowledge in `CONTEXT.md`, `docs/adr/` and `TODO.md`; an agent-written store beside those would give the
same facts two sources of truth. `.serena/project.yml`'s `initial_prompt` points Serena at those files
instead.

## Status

✅ **Functionally complete** — 16 debug tools, integrated and validated against a live JVM.

### Implemented
- [x] JDWP protocol (handshake, packets, encoding/decoding)
- [x] MCP server with 16 debug tools (stdio transport)
- [x] VirtualMachine commands (Version, IDSizes, AllThreads, Suspend/Resume, CreateString)
- [x] ClassesBySignature, ReferenceType.Methods/Fields/Signature, ClassType.Superclass
- [x] Method.LineTable / VariableTable
- [x] EventRequest.Set/Clear/ClearAllBreakpoints — breakpoints with location, **count**, **thread**, **exception**, and **field** modifiers
- [x] ThreadReference.Frames, StackFrame.GetValues/SetValues/ThisObject
- [x] ObjectReference.ReferenceType/GetValues/**InvokeMethod**, ClassType.**InvokeMethod** (statics), ArrayReference.Length/GetValues, StringReference.Value
- [x] **Event loop** for async breakpoint/step notifications
- [x] **Stepping** (step over/into/out)
- [x] **Expression evaluation** — `var`/`this`/`Class` + `.field` / `.method(args)` chains, superclass walk
- [x] **Static-method invocation** — `Class.staticMethod(args)`, restricted to `ACC_STATIC` overloads
- [x] **Object arguments** — pass a local, `this`, or a nested expression by reference; overloads resolved
      against each argument's runtime class chain (so `pick(Item)` beats `pick(Object)`), and a
      type-mismatched invoke is refused rather than handed to the JVM
- [x] **String and object dereferencing**, array contents, best-effort `toString()`, source-line resolution
- [x] **Recursive object expansion** — bounded depth/breadth + node budget, cycle detection, boxed-wrapper
      unboxing, and element-level `List`/`Set`/`Map`/`Optional` rendering (`expand_objects`)
- [x] **Type cache** — per-connection memo of each loaded type's signature/fields/methods/superclass;
      48% fewer JDWP packets on a cold deep expansion, 62% warm (values are never cached)
- [x] **Collection subscripts** — index, `Map` key lookup (with key boxing), half-open slice, and
      predicate filter with element-relative left sides; bounded by a documented scan cap
- [x] **Conditional breakpoints** — `condition` evaluated in the hit frame (`expr OP expr` or boolean chains); auto-resumes when false
- [x] **Multiple concurrent sessions** — `debug.attach` returns a `session_id`; tools take an optional `session_id` (defaults to current); `debug.list_sessions` finds one you lost
- [x] **Arguments** in `evaluate` / conditions: literals (int, long `123L`, boolean, null, `"string"`) or expressions
- [x] **Field watchpoints** — `FIELD_MODIFICATION` / `FIELD_ACCESS` requests; a write hit reports the
      mutating location and the old → new pair (the old value is read before the pending store commits)
- [x] **Safety**: `panic` + idle watchdog auto-resume (clears breakpoints, exception breakpoints and watchpoints)
- [x] **Performance**: type cache, `package_filter`, single-threaded `invoke_method`, token-trimmed output
- [x] Architecture independence (big-endian protocol; Intel & ARM)

## References

- [JDWP Specification](https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html)
- [Model Context Protocol](https://modelcontextprotocol.io/)
- [Claude Code MCP Documentation](https://docs.claude.com/claude-code)

## License

MIT
