# jdwp-mcp

A native **JDWP debugger exposed as an MCP server** — attach to a running JVM and set stop points, read
stacks and variables, evaluate expressions, step, hot-reload classes, and trace without suspending
anything.

```bash
npx jdwp-mcp
```

It speaks MCP over stdio, so point your client at that command:

```json
{
  "mcpServers": {
    "jdwp": { "command": "npx", "args": ["-y", "jdwp-mcp"] }
  }
}
```

No JDK is needed to run the debugger itself — it is one self-contained binary that speaks the JDWP wire
protocol directly. The JVM you are debugging needs to have been started with `-agentlib:jdwp`.

## What you get

42 `debug.*` tools. The ones worth knowing about first:

- `debug.attach` — connect to a JVM by host and port. **Settle whose JVM it is first**: on a shared app
  server every suspension freezes other people's in-flight requests, and the tools are built around not
  doing that to you by accident.
- `debug.set_line_stop` — a breakpoint, optionally with a condition, a hit count, a thread filter, or a
  wildcard class pattern that also arms classes loading later. Pass `trace: true` to make it a
  **non-suspending logpoint** that snapshots and resumes.
- `debug.get_stack`, `debug.evaluate`, `debug.evaluate_chain` — inspect frames and evaluate expressions
  against them, with overload resolution on runtime types.
- `debug.get_traces` — read what the non-suspending stop points captured.
- `debug.panic` — clear everything and resume, when you need the JVM back.

The full list is in the [tool reference](https://github.com/YgorPerez/java-debugging-mcp/blob/main/docs/tools.md).

## Platforms

Prebuilt binaries ship for Linux x64/arm64, macOS arm64/x64 and Windows x64, as `optionalDependencies` —
so installing fetches exactly one of them and there is **no download at run time**. Anything else builds
from source and is just as supported:

```bash
cargo install jdwp-mcp
```

## Safety

The JVM you attach to is usually somebody else's. Two guards exist for that: `read_only: true` (or
`JDWP_READONLY`) refuses method invocation, `set_value`, `force_return` and `reload_class` while leaving
every read working; and a watchdog auto-resumes a VM left suspended after `JDWP_WATCHDOG_SECS` (default
120), disabling whatever froze it so the rescue cannot be undone by the next hit.

Neither is a security boundary — anyone who can reach a JDWP port owns the JVM.

MIT licensed. Source, issues and the full documentation:
<https://github.com/YgorPerez/java-debugging-mcp>.
