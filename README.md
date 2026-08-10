# jdwp-mcp

**Java debugging for LLMs via JDWP and Model Context Protocol**

An MCP server that lets Claude Code and other LLM tools debug Java applications over the Java Debug
Wire Protocol. Attach to a running JVM — or launch one — set stop points, read live objects, evaluate
expressions, step, and hot-swap code, all in natural language.

It speaks JDWP natively in Rust, so it is **one self-contained binary**: no JDK, no JDI, no agent to
install in the target.

📖 **[Tool reference](docs/tools.md)** · 🛠 **[Development](docs/development.md)** ·
⚖️ **[Compared with the other Java debugging MCP servers](docs/comparison.md)**

## What it is good at

**It is built for a JVM you are not allowed to freeze.** That constraint shapes everything else: a
shared app server serving other people's requests, reached over a `kubectl port-forward`, where the
usual debugger move — suspend everything and poke around — is an outage.

- **Non-suspending trace mode.** `trace:true` on a breakpoint, exception stop, watchpoint or
  method-exit stop snapshots the hit and resumes the thread immediately. Each snapshot carries the
  calling chain above it, so a logpoint answers *which path reached this*. Read them with
  `debug.get_traces`.
- **Freeze one thread, not the VM.** `debug.suspend_thread` holds a single worker and leaves the rest
  serving — enough for the whole stack, locals, field chains and deep object expansion.
- **A watchdog that undoes your mistake.** A VM or thread left suspended too long is auto-resumed
  (`JDWP_WATCHDOG_SECS`, default 120) and whatever froze it is *disabled*, so it cannot re-freeze on
  the next hit. `debug.panic` does it on demand; `debug.disconnect` can never leave a JVM frozen.
- **Read-only sessions.** `JDWP_READONLY=1` refuses everything that would execute or install code —
  enforced at the JDWP boundary, so the indirect paths (`toString()` rendering, `Map` subscripts,
  breakpoint conditions) are covered too.
- **Costs are measured and reported, never guessed.** A thread dump says how long it held the VM and
  how many packets it spent. A traced stop point reports its own capture cost *on your JVM*. A heap
  query states the pause it imposed. A budget that truncates says what it dropped.

**Expression evaluation that resolves like `javac` does.** `localVar` / `this` / `Class` / `@0x…`
heads with `.field` and `.method(args)` chains, including static members. Overloads resolve on the
arguments' **runtime types** — interfaces walked transitively, autoboxing applied, and an argument a
parameter cannot accept is refused rather than handed to the JVM. Arguments may be literals or
expressions passed by reference (`svc.matches(reserva)`).

- **Collections as first-class syntax** — `lines[0]`, `counts["key"]`, `lines[2..5]`, and
  `lines[?qty > 3]` filters with the left side resolved against each element. A filter reports
  `N of M matched`, so an empty result is distinguishable from an unscanned one.
- **Reads that need no suspended thread.** A subscript, slice or filter on a `HashMap`,
  `LinkedHashMap`, `ConcurrentHashMap` or `ArrayList` is answered by walking the collection's own
  fields instead of invoking `get()` in the debuggee — so the commonest cache question works under
  `read_only` on a JVM you must not freeze. Any other implementation falls back to invoking, and the
  reply says which path it took.
- **Deep expansion** — `expand_objects:true` walks nested objects, arrays and `List`/`Set`/`Map`/
  `Optional` contents into a field tree, bounded by depth, breadth and a node budget, with cycle
  detection and unboxed wrappers.
- **`byte[]` as text** — `byte[73] ISO-8859-1 "<?xml version=…"` rather than a list of signed
  integers, with a trailing `#<charset>` to pick the reading. Octets that do not decode are marked
  `\xNN`, so a wrong charset looks wrong instead of looking like a bug in the payload.

**Questions that otherwise cost ten tool calls, answered in one.**

| The question | The tool |
| --- | --- |
| Which link in this chain went null? | `debug.evaluate_chain` — names the first null, values above it, and how many links it never reached |
| What did this method return, and from which `return`? | `debug.set_method_exit_stop` |
| Who changes this field behind my back? | `debug.set_field_stop` — reports the mutating location with **old → new** |
| Requests are hanging — which threads are blocked on what? | `debug.set_monitor_stop` (live, no suspend) or `debug.thread_dump` (names the lock *owner*) |
| Is this JVM even running the code I compiled? | `debug.check_stale` — compares line tables method by method |
| Where does this object live if nothing on the stack names it? | `debug.list_instances` — live objects of a type as `@0x…` handles |
| Does this `@NamedQuery` return what its author believes? | `debug.run_named_query` — through the app's own `EntityManager`, without the flush it would have caused |

**An exception hit reports its message.** On JDK 15+ that is frequently the whole diagnosis: the JVM
has already computed *`because the return value of "X.getY()" is null`*, naming the failing
subexpression a hand-run bisect would have taken three calls to find. On a framework that rethrows,
the sightings of one instance are **folded** — original throw and escape point kept, the plumbing
between them a count.

**Hot reload, and the frame rewind that makes it work.** `debug.reload_class` installs freshly
compiled bytecode into the running JVM — no redeploy, no restart, warm state intact — and a request
suspended at a breakpoint survives the fix: swap the method, `debug.pop_frame`, `debug.continue`, and
it re-runs with the new code. HotSpot accepts method bodies only, and each of the twelve ways it can
refuse is turned into what to do next instead of a bare error code.

**Stepping that lands on your code.** `step_into` skips the JDK and the container by default
(`java.*`, `jakarta.*`, `org.jboss.*`, `io.undertow.*`, `org.hibernate.*`, …), so you arrive at the
next line of *your* method rather than inside a Weld proxy. `exclude_classes:[]` restores the
unfiltered behaviour, `only_classes` is the inverse, and every step reply says which was in force.

**Discovery, for when you do not know the name yet.** `debug.list_classes` shows what the debuggee has
actually loaded — the only way to find a generated proxy or a shaded class. `debug.list_methods` and
`debug.list_fields` render signatures as Java source types with generics. `debug.source` settles
whether your checkout is the code that is running.

**42 tools in total** — see the [tool reference](docs/tools.md).

## Install

### 1. Start your Java app with JDWP enabled

```bash
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005 -jar myapp.jar
```

Or skip this and let `debug.launch` start it for you — which is the only way to break on code that
runs *during* startup, since it holds the VM before its first instruction.

### 2. Get the server

**`npx`, if you have Node 18 or newer** — nothing to install, nothing to download at run time:

```bash
npx -y jdwp-mcp
```

The prebuilt binary arrives as an `optionalDependencies` package, so npm fetches exactly the one for your
platform and there is no fetch when the server starts. Linux x64/arm64, macOS arm64/x64 and Windows x64
are covered; anything else falls through to `cargo install` below, and the launcher says so rather than
failing obscurely.

**Or download a prebuilt binary** — no Node and no Rust toolchain needed — from the
[latest release](https://github.com/YgorPerez/java-debugging-mcp/releases/latest):

| Platform | Asset |
| --- | --- |
| Linux x86_64 | `jdwp-mcp-<tag>-linux-x86_64` |
| Linux ARM64 | `jdwp-mcp-<tag>-linux-aarch64` |
| macOS (Apple Silicon) | `jdwp-mcp-<tag>-macos-aarch64` |
| macOS (Intel) | `jdwp-mcp-<tag>-macos-x86_64` |
| Windows x86_64 | `jdwp-mcp-<tag>-windows-x86_64.exe` |

Both Linux builds are statically linked against musl, so they run on any Linux of that architecture
whatever the distribution's glibc — including an app server older than the machine you downloaded it on.
Every release ships a `SHA256SUMS` covering all five assets:

```bash
tag=v0.17.0
base=https://github.com/YgorPerez/java-debugging-mcp/releases/download/$tag
curl -LO "$base/jdwp-mcp-$tag-linux-x86_64" && curl -LO "$base/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS   # macOS: shasum -a 256 -c SHA256SUMS --ignore-missing
chmod +x "jdwp-mcp-$tag-linux-x86_64"
```

That checksum proves the **download** arrived intact, not who built it: the manifest ships beside the
binaries, so anything able to replace one could replace the other. Every asset also carries a signed
build provenance statement naming the workflow, commit and run that produced it, which is the half a
checksum cannot answer:

```bash
gh attestation verify "jdwp-mcp-$tag-linux-x86_64" --repo YgorPerez/java-debugging-mcp
```

The macOS binaries are unsigned, so the first run needs `xattr -d com.apple.quarantine <file>` or
Settings → Privacy & Security → "Open Anyway".

**Or install from crates.io** — needs Rust 1.85 or newer, and compiles from source, so budget a few
minutes the prebuilt binaries above do not cost you:

```bash
cargo install jdwp-mcp     # binary at ~/.cargo/bin/jdwp-mcp
```

**Or build from a clone** — same 1.85 floor:

```bash
cargo build --release   # binary at target/release/jdwp-mcp
```

The floor is 1.85 because it was *measured* rather than declared (BUILD-2): 1.82 and 1.83 fail on this
workspace's own code, and 1.84 fails at dependency resolution before any of it is compiled.

### 3. Configure Claude Code

```bash
# From your Java project directory
claude mcp add --scope project jdwp /path/to/jdwp-mcp
```

`--scope project` makes the debugger available only in this project. Manual configuration via
`.mcp.json` works too:

```json
{
  "mcpServers": {
    "jdwp": {
      "command": "/path/to/jdwp-mcp",
      "env": { "JDWP_READONLY": "0", "JDWP_WATCHDOG_SECS": "120" }
    }
  }
}
```

Or with `npx`, if you installed it that way — no path to keep current:

```json
{
  "mcpServers": {
    "jdwp": {
      "command": "npx",
      "args": ["-y", "jdwp-mcp"],
      "env": { "JDWP_READONLY": "0", "JDWP_WATCHDOG_SECS": "120" }
    }
  }
}
```

`JDWP_CLASS_ROOTS` and `JDWP_SOURCE_ROOTS` are worth setting too — they are what `debug.check_stale`,
`debug.reload_class` and `debug.source` read, and without a class root the arm-time staleness check has
nothing to compare against, so every stop point you arm says `Staleness NOT CHECKED` instead of vouching
for the line it just resolved (DISC-14).

## Use it

```
> Attach to the JVM at localhost:5005
> Set a breakpoint at com.example.HelloController line 65
> When it hits, show me the stack and the value of requestCount
```

On a **shared** instance, ask for trace mode instead of a suspending stop point:

```
> Trace com.example.OrderService.submit without suspending, and show me the caller chain
> Read the traces
```

For a Kubernetes-deployed app, forward the JDWP port first:

```bash
kubectl port-forward pod/my-app-pod 5005:5005
```

Most tools take `thread_id` as an optional hex string (e.g. `"0x2"`); when omitted they default to the
last thread that hit a breakpoint. Every tool takes an optional `session_id` — `debug.attach` can hold
several JVMs at once and `debug.list_sessions` finds one you lost.

Worked examples with captured output are in [`examples/`](examples/README.md).

## Two things that look like reads and are not

Neither is caught by `read_only`, which is why they are written down rather than guarded.

- **A JAX-RS `Response` entity is single-pass.** `response.readEntity(String.class)` **consumes** it,
  and the application's own read afterwards gets an empty body — you break the thing you were
  inspecting by inspecting it. Break at or after the assignment to a local instead, or capture the
  value with `debug.set_method_exit_stop`. The same goes for any one-shot stream.
- **`debug.list_instances` looks free and is not.** JDWP needs no suspend and none is issued, yet the
  JVM stops the world for a full live-heap walk — measured at **522 ms of held application threads on
  a 2,000,000-object heap to answer with 7 objects**. Nothing refuses on size; the reply states the
  duration it actually held them.

Both are covered in full, with the rest of the shared-instance rules, in the
[tool reference](docs/tools.md).

## Status

✅ **Functionally complete** — 38 debug tools, integrated and validated against live JVMs on JDK 11,
17 and 21.

The JDWP client implements the VirtualMachine, ReferenceType, ClassType, Method, ObjectReference,
StringReference, ArrayReference, ThreadReference, StackFrame and EventRequest command sets this needs,
including `RedefineClasses`, `PopFrames`, `InvokeMethod` (instance and static), `Instances` /
`InstanceCounts`, and the four `MONITOR_*` events — big-endian throughout, so Intel and ARM both work.

Known open issues are tracked as [GitHub issues](https://github.com/YgorPerez/java-debugging-mcp/issues),
which are the authority on open work. [`docs/adr/`](docs/adr/) holds the decisions that are settled, and each
release's notes carry the commit body of what shipped and why.

## References

- [JDWP Specification](https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html)
- [Model Context Protocol](https://modelcontextprotocol.io/)
- [Claude Code MCP Documentation](https://docs.claude.com/claude-code)

## License

MIT
