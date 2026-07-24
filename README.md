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
  overloads by the arguments' **runtime types** and walking the superclass chain. Arguments are
  literals (int, long, boolean, null, `"string"`) **or expressions passed by reference**
  (`svc.matches(reserva)`, `foo.handle(this, cfg.getId())`)
- **Value Rendering**: Strings, typed objects (best-effort `toString()`), and **array contents**
- **Recursive Expansion**: `expand_objects:true` on `debug.evaluate` / `debug.get_stack` walks nested
  objects, arrays, and **`List`/`Set`/`Map`/`Optional` contents** into a field tree — bounded by
  `max_depth`/`max_children` and a node budget, with **cycle detection** and unboxed wrappers
- **Field Watchpoints**: break when a field is read or written — `debug.set_watchpoint` reports the
  mutating location with the **old → new** value, for "who changes this behind my back?"
- **Set Values**: write a local variable in a suspended frame
- **Thread Management**: tools default to the last thread that hit a breakpoint
- **Structured Events**: `get_last_event` emits a machine-readable `[event]` line (thread, class.method:line)
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
| `debug.set_breakpoint` | Set a breakpoint by class+line, or by method name; optional `hit_count`, thread filter, `condition`, or `trace:true` (non-suspending logpoint) |
| `debug.set_exception_breakpoint` | Break when an exception (of a class + its subclasses, or all) is thrown; `caught`/`uncaught` selectable |
| `debug.get_traces` | Read snapshots captured by trace/logpoint breakpoints (bounded ring buffer; optional `clear`) |
| `debug.list_breakpoints` | List active breakpoints (line, deferred, exception) |
| `debug.clear_breakpoint` | Remove a breakpoint (line, deferred, or exception) |
| `debug.continue` | Resume execution |
| `debug.step_over` | Step over current line (defaults to last-hit thread) |
| `debug.step_into` | Step into a method call |
| `debug.step_out` | Step out of the current method |
| `debug.get_stack` | Stack frames, compact `#i class.method:line` with typed locals indented |
| `debug.evaluate` | Evaluate `var`/`this`/`Class` + `.field` / `.method(args)` chains in a frame; static methods and object arguments included; `expand_objects` for a deep field tree |
| `debug.set_value` | Write a local variable, a static field (`Class.field`), or an instance field (`this.field`) |
| `debug.set_watchpoint` | Break when a field is written (or read) — reports the mutating location + old → new value |
| `debug.force_return` | Force the current method to return a given value, skipping the rest of its body |
| `debug.get_last_event` | Last event as a machine-readable `[event]` line (thread, class.method:line, exception type, watched field's old → new) + `[suspended]` |
| `debug.list_threads` | List threads by name; filter with `name_filter` / `only_suspended` / `limit` |
| `debug.pause` | Pause execution (suspend all threads) |
| `debug.panic` | Safety: clear all breakpoints and resume all threads |
| `debug.disconnect` | End the debug session |

Most tools take `thread_id` as an optional hex string (e.g. `"0x2"`); when omitted they default to
the last thread that hit a breakpoint. A watchdog auto-resumes a VM left suspended for too long
(`JDWP_WATCHDOG_SECS`, default 120).

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
cargo test                      # unit tests (fast, no JVM)
scripts/integration-test.sh     # MCP-level: the real binary over JSON-RPC against probe JVMs
scripts/doctor.sh               # the rust-doctor health gate CI runs
```

`scripts/integration-test.sh` runs `mcp-server/tests/mcp_integration.rs`, which launches and reaps its
own probe JVMs from `examples/probes/` — no manual steps. It does need a JDK: without one every test
prints `SKIP` and passes, so check for `SKIP` lines before reading a green run as coverage.

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
- [x] **Conditional breakpoints** — `condition` evaluated in the hit frame (`expr OP expr` or boolean chains); auto-resumes when false
- [x] **Multiple concurrent sessions** — `debug.attach` returns a `session_id`; tools take an optional `session_id` (defaults to current)
- [x] **Arguments** in `evaluate` / conditions: literals (int, long `123L`, boolean, null, `"string"`) or expressions
- [x] **Field watchpoints** — `FIELD_MODIFICATION` / `FIELD_ACCESS` requests; a write hit reports the
      mutating location and the old → new pair (the old value is read before the pending store commits)
- [x] **Safety**: `panic` + idle watchdog auto-resume (clears breakpoints, exception breakpoints and watchpoints)
- [x] Architecture independence (big-endian protocol; Intel & ARM)

## References

- [JDWP Specification](https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html)
- [Model Context Protocol](https://modelcontextprotocol.io/)
- [Claude Code MCP Documentation](https://docs.claude.com/claude-code)

## License

MIT
