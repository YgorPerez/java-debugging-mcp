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
- **Which link went null**: `debug.evaluate_chain` walks a chained expression left to right and names the
  first link that is null, with every link's value above it and a count of the ones it never reached —
  the one-call answer to a question that otherwise costs one `debug.evaluate` per link. Each method in
  the chain runs exactly once. For the case where nothing *throws*; when it does, the exception's own
  message is better (below)
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
  rate hits are arriving at, and the share of the window spent capturing (invert the mean for the rate past
  which hits queue). A traced stop point that has captured nothing says so, rather than reporting zero
- **Hot reload**: `debug.reload_class` installs freshly compiled bytecode into the running JVM
  (`RedefineClasses`) — no redeploy, no restart, warm state intact, and a request suspended at a
  breakpoint survives the fix: swap the method, `debug.pop_frame`, `debug.continue`, and it re-runs with
  the new code without re-issuing the call that got you there. HotSpot accepts **method bodies only**,
  and each of the twelve ways it can refuse is turned into what to do next instead of a bare error code
- **Staleness detection**: `debug.check_stale` answers whether the JVM is running the build on your
  disk, by comparing line tables method by method — the failure that otherwise costs twenty tool calls
  debugging the *program* while the deployed bytecode is last week's. With a class root configured,
  `debug.set_line_stop` also reports it **unasked** when the method you just armed has drifted (DISC-8),
  since the caller this ruins is the one who never thought to check; it speaks only when it has a proof,
  so a quiet reply is not a promise that your build is current. `bytecode:true` adds the evidence line
  tables cannot give (DISC-9) — a body edit that moved no line, like `x < 5` to `x <= 5` — and is the only
  one that answers at all on a `-g:none` build
- **Thread Management**: tools default to the last thread that hit a breakpoint
- **Thread dumps with lock ownership**: `debug.thread_dump` answers "it's wedged — which threads are
  blocked on what?" in one call: every thread's stack, the monitors it holds, the one it is blocked
  entering, and **who holds that** — so a deadlock cycle is visible without leaving the debugger. When it
  cannot show every thread it picks them **one per thread-name family**, not the first `limit` the JVM
  listed, because that is creation order and an app server creates its request pool last (ADR-0013).
  `debug.list_threads` truncates by the same rule and says so in the same words, so the cheap call you run
  to decide what to dump does not show you a different population than the dump will
- **Structured Events**: `get_last_event` emits a machine-readable `[event]` line (thread, class.method:line),
  from a bounded buffer — a burst of hits isn't lost, and the reply says how many are still pending
- **An exception hit reports its message**, not just its type and location. On JDK 15+ that is frequently
  the whole diagnosis: the JVM has already computed *`because the return value of "X.getY()" is null`*,
  which names the failing subexpression a hand-run bisect would have taken three calls to find. Available
  in trace mode too. And on a framework that rethrows — an EJB interceptor chain, a Spring proxy — the
  sightings of one instance are **folded** rather than recorded 30 times: the original throw and the point
  where it escapes are both kept, the plumbing between them becomes a count, and a collapsed rethrow does
  not spend `trace_max_hits`
- **Safety**: a `panic` tool (clear all + resume) and a **watchdog** that auto-resumes a long-suspended
  VM (`JDWP_WATCHDOG_SECS`, default 120) so a forgotten breakpoint can't freeze a shared instance

> This fork implements `debug.evaluate` and `debug.step_*` (stubs upstream) plus the safety,
> structured-event, array, set-value, and breakpoint-modifier features above. See
> [Compared with the other Java debugging MCP servers](#compared-with-the-other-java-debugging-mcp-servers).

## Quick Start

### 1. Start your Java app with JDWP enabled

```bash
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005 -jar myapp.jar
```

### 2. Get the MCP server

**Download a prebuilt binary** — no Rust toolchain needed — from the
[latest release](https://github.com/YgorPerez/java-debugging-mcp/releases/latest):

| Platform | Asset |
| --- | --- |
| Linux x86_64 | `jdwp-mcp-<tag>-linux-x86_64` |
| macOS (Apple Silicon) | `jdwp-mcp-<tag>-macos-aarch64` |
| macOS (Intel) | `jdwp-mcp-<tag>-macos-x86_64` |
| Windows x86_64 | `jdwp-mcp-<tag>-windows-x86_64.exe` |

The Linux build is statically linked against musl, so it runs on any x86_64 Linux whatever the
distribution's glibc — including an app server older than the machine you downloaded it on. Every
release ships a `SHA256SUMS` covering all four assets:

```bash
tag=v0.1.0
base=https://github.com/YgorPerez/java-debugging-mcp/releases/download/$tag
curl -LO "$base/jdwp-mcp-$tag-linux-x86_64" && curl -LO "$base/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS   # macOS: shasum -a 256 -c SHA256SUMS --ignore-missing
chmod +x "jdwp-mcp-$tag-linux-x86_64"
```

The macOS binaries are unsigned, so the first run needs `xattr -d com.apple.quarantine <file>` or
Settings → Privacy & Security → "Open Anyway".

**Or build from source** — needs Rust 1.82 or newer:

```bash
cargo build --release   # binary at target/release/jdwp-mcp
```

### 3. Configure Claude Code

The easiest way to enable the MCP server for your project:

```bash
# From your Java project directory
claude mcp add --scope project jdwp /path/to/jdwp-mcp/target/release/jdwp-mcp
```

Adjust the path to match where you saved the downloaded binary (or `target/release/jdwp-mcp` if you built from source). The `--scope project` flag makes the debugger available only in your current Java project.

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
| `debug.launch` | **Start** a JVM under the debugger and attach to it — `main_class` + `classpath`, or `jar`, plus optional `jvm_args` / `args` / `working_dir` / `java_home`. `suspend` defaults to true, holding the VM before its first instruction, which is the only way to break on code that runs during startup. `debug.disconnect` terminates it unless `detach_on_disconnect:true`; its stdout/stderr are captured (never inherited — this server's stdout is the MCP transport) and reported if it dies |
| `debug.set_line_stop` | Set a breakpoint by class+line, or by method name. `class_pattern` takes an exact class, a **wildcard** (`com.example.*` — arms one breakpoint per matching loaded class, keeps arming ones that load later, and is clearable as one `bpset_` id; needs `method`, refuses `line`, bounded by `max_classes` — a family that is **full** parks its class-load watch, so it costs the JVM nothing until a cleared member frees a slot), or a **list** of either; optional `hit_count`, thread filter, `condition` (with `&&`/`||`), or `trace:true` (non-suspending logpoint, with `trace_max_hits` and `trace_frames`) |
| `debug.set_exception_stop` | Break when an exception (of a class + its subclasses, or all) is thrown; `caught`/`uncaught` selectable, an optional `thread_id` filter, or `trace:true` (with `trace_max_hits` / `trace_frames`) to collect throws without suspending. Each hit reports the exception's **message** as well as its type and catch site — on JDK 15+ that is usually the diagnosis itself, since a `NullPointerException` names the failing subexpression (`because the return value of "X.getY()" is null`). On a framework that rethrows, the sightings of one instance are **folded**: the original throw and the escape point are both kept, the layers between become a count, and a collapsed rethrow does not spend `trace_max_hits` |
| `debug.get_traces` | Read snapshots captured by any `trace:true` stop point — line, exception or watchpoint, each with the caller chain above it (bounded ring buffer; narrow with `bp_id` / `class_filter` / `since`, optional `clear`). A record marked `↻ rethrow of #<seq>` is the escaping end of a rethrow chain, and `#<seq>` is the original throw — the one with the application frame and the cause |
| `debug.list_stop_points` | List active stop points (line, deferred, exception, watchpoint, method-exit) with trace budgets and thread filters — plus, for each traced one, its **measured** capture cost: mean per hit, the rate hits are arriving at, and the share of the window spent capturing |
| `debug.clear_stop_point` | Remove a stop point (line, deferred, exception, watchpoint, method-exit) — or a whole wildcard family by its `bpset_…` id, which also drops its watch for classes that load later |
| `debug.toggle_stop_point` | Silence or re-arm any stop point (`bp_…` / `exc_…` / `watch_…` / `mexit_…` / `bpset_…`) without losing its `condition`/`trace_expr`; the id stays the same across the round trip |
| `debug.continue` | Resume execution |
| `debug.step_over` | Step over current line (defaults to last-hit thread) |
| `debug.step_into` | Step into a method call |
| `debug.step_out` | Step out of the current method |
| `debug.get_stack` | Stack frames, compact `#i class.method:line` with typed locals indented |
| `debug.evaluate` | Evaluate `var`/`this`/`Class` + `.field` / `.method(args)` chains in a frame; static methods, object arguments, `[i]`/`["k"]`/`[a..b]`/`[?pred]` subscripts (predicates support `&&`/`||`), and `expand_objects` for a deep field tree |
| `debug.evaluate_chain` | **Which link went null?** Walks the same chained expression `debug.evaluate` takes, link by link, printing each one's value and naming the first null — plus how many links after it were never evaluated. For a chain that returns null or an empty collection without throwing, which otherwise costs one `debug.evaluate` per link, bisected by hand. Every method in the chain runs **exactly once** (each link resolves against the previous link's value, not by re-evaluating longer prefixes) and no `toString()` is invoked. A separate tool rather than a flag, per ADR-0015 — "where did this become null" is a different question from "what is this value". If the chain *throws*, prefer the exception's own message: on JDK 15+ it names the failing subexpression |
| `debug.set_value` | Write a local variable, a static field (`Class.field`), an instance field (`this.field`), or one element (`xs[0]`, `counts["k"]`) — from a literal or a copied live reference (`this.a = other.b`) |
| `debug.set_field_stop` | Break when a field is written (or read) — reports the mutating location + old → new value; optional `thread_id` filter; `trace:true` (with `trace_max_hits` / `trace_frames`) collects hits without suspending |
| `debug.set_method_exit_stop` | Report what a method **returned**, and from which `return` — for a method with several exits, or a value from a chain you can't break on. `class_pattern` + `method`; `trace` defaults to **true** here (a suspending method exit on a hot method freezes a VM fastest), and a broad suspending request is refused with the reason |
| `debug.force_return` | Force the current method to return a given value, skipping the rest of its body |
| `debug.reload_class` | **Hot reload**: ship a freshly compiled `.class` into the running JVM (`VirtualMachine.RedefineClasses` — what an IDE calls "reload changed classes"), with no redeploy and no restart. Warm state, pools, app context and any in-flight request survive, including one suspended at a breakpoint. Compiling stays yours; this reads the output, at `<class root>/<package as directories>/<SimpleName>.class` from `debug.attach {class_roots:[…]}`, `JDWP_CLASS_ROOTS`, the call, or an explicit `class_file`. HotSpot takes **method bodies only** — add a method or a field, change a modifier or the hierarchy, and the reply names which of those you did and says a redeploy is the only route, rather than leaving a bare `SCHEMA_CHANGE_NOT_IMPLEMENTED` to be re-tried forever. A refusal changes nothing (redefinition is all-or-nothing). Reports whether the thread you are stopped on is *inside* the class, since a frame already on the stack keeps the bytecode it entered with. `dry_run:true` sends nothing; refused read-only |
| `debug.pop_frame` | Rewind a suspended thread to the **call site** of a frame (`StackFrame.PopFrames`), so `debug.continue` re-executes the call. The other half of a hot reload — a frame already running keeps its old bytecode, so a swap of the method you are stopped in looks like it did nothing until the frame is popped — and useful alone for re-running a method you stepped past. Every frame above the named one goes too (JDWP's behaviour, not a convenience). Side effects are **not** undone; refused read-only |
| `debug.get_last_event` | Last event as a machine-readable `[event]` line (thread, class.method:line, exception type **and message**, watched field's old → new) + `[suspended]`; events are buffered, so `limit` reads a backlog and `drain` discards it. A watchdog rescue is reported only against the suspension it actually ended, so an old auto-resume is never replayed beside a live hit |
| `debug.list_threads` | List threads by name; filter with `name_filter` / `only_suspended` / `limit`. A listing too big for `limit` picks **one thread per name family** rather than the first `limit` in JDWP's creation order — the same rule as `debug.thread_dump`, stated in the reply the same way (ADR-0013). It reads one packet per thread *name* to do that: measured on `ChurnProbe`, 103 packets against the 381 of a dump of the same JVM, and it reaches all 8 workers the debuggee starts last where the old creation-order listing reached 0 |
| `debug.list_classes` | What the debuggee has actually **loaded** — the first step when you don't already know the FQN a stop point needs, and the only way to find a generated proxy, a shaded class, or a deployment that differs from your checkout. `filter` takes `com.example.*`, `*.OrderService` or a bare substring, matched against the dotted name. Bounded: the reply reports matched-against-loaded rather than dumping thousands of types. Arrays excluded unless `include_arrays:true` |
| `debug.list_methods` | A class's methods with signatures rendered as **Java source types** (`static boolean matches(java.lang.String, int)`) — what you need to compose a `debug.evaluate` call, since overloads resolve on the runtime types you supply. All overloads listed; `static`/`abstract`/`native` marked (the latter two have no body to break on). Declared-only unless `inherited:true` walks the superclass chain, attributing each row |
| `debug.list_fields` | What state a class **holds**, for when you have a type and no instance to expand — a static holder, a class you're about to breakpoint into, a vendored or shaded class the checkout can't show you. Rendered as a Java declaration (`static final java.lang.String INFRA`, `int qty`), so static and instance are told apart at a glance; `final` and `volatile` are marked too. **Statics are listed first** — they're the ones `debug.evaluate` reads with no instance and no suspended thread. Declared-only unless `inherited:true` walks the superclass chain, attributing each row. Reads no *values*; bounded like the other discovery tools. A class that declares nothing says so as an answer rather than looking like a failed lookup (ADR-0015) |
| `debug.source` | What file a class was **compiled from**, and optionally the source lines around one. Two independent halves: the JVM half needs no local files and is what settles whether your checkout is the code actually running — a class reporting `Order.java` in a tree where that file was renamed months ago is the answer, and reading local source would never have shown it. A JSR-45 source debug extension (JSP, Kotlin, Groovy) is reported when the class carries one, meaning the `.java` name is only an intermediate. The disk half turns a frame's `class.method:line` into text: pass `line` for a bounded window (`context`, default 20 either side) rather than pulling a whole file into context; `whole_file:true` is capped by `max_lines` (default 400) and the reply always states which lines of how many. Roots come from `debug.attach {source_roots:[…]}`, `JDWP_SOURCE_ROOTS`, or the call itself, and a root is where the **package tree** starts. Not loaded / compiled with no `SourceFile` / no root holds it / found-but-unreadable stay four distinct answers |
| `debug.check_stale` | **Is this JVM running the code you just compiled?** Compares the JVM's per-method line tables against the ones in your `.class`, from the same roots `debug.reload_class` reads. The half `debug.source` cannot answer: `SourceFile` is a compile-time string, identical across every build of the file, so it settles *which file* and never *which build of it* — and same-class-same-file-older-bytecode is the case a redeploy loop actually produces. It catches an edit that **moved a line** (which is what makes a stop point at `:412` mean something else) and is blind to one that moves none, so a clean result means "no line moved", not "byte-for-byte identical", and says so. A method on one side only is reported apart, as a class-shape change a hot reload could not fix; a class with no line tables at all (`-g:none`, an interface) reports **cannot tell** rather than a match |
| `debug.thread_dump` | Every thread's stack in one call **plus** the monitors each holds and the one it is blocked entering, with the blocker named (`← held by 0x<id> "<name>"`) — a deadlock cycle is readable straight off it. JDWP can only read a *suspended* thread, so pass `suspend:true` (freeze, read, resume, verify) or `only_suspended:true`; it never suspends on its own. Bound the cost with `name_filter` / `limit` / `max_frames` / `package_filter`, and the freeze with `max_suspend_ms` (default 2000) — the reply reports how long it held the VM, the packets it spent, and any threads a budget made it skip. A dump too big for `limit` chooses **one thread per name family** (digits ignored) rather than the first `limit` in JDWP order, states that rule in its header, and names the groups it withheld (ADR-0013). `monitors_only:true` reads the lock graph without the stacks for a fraction of the freeze (measured: 245 packets / 33 ms against 770 / 117 ms on a 60-thread dump) |
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
everything that would execute code in or install code into the target: method invocation, writes,
`force_return`, `debug.pop_frame`, and `debug.reload_class` — on a shared instance a redefinition is an
unannounced deploy, not a debugger read, so it is refused first and `dry_run:true` is the one part that
still answers.
Enforced on the connection itself rather than by inspecting expressions, so the indirect paths are
covered too — `toString()` rendering, `List`/`Map` subscripts, and breakpoint `condition`/`trace_expr`
(refused when you arm them, not silently on each hit). The honest cost is shallower output: objects
render as `Type @0x…`, because pretty-printing one means invoking it — and that `@0x…` is a handle
`debug.evaluate` accepts as an expression head, so a shallow render is still somewhere to go. Reads that need no
invocation are unaffected — locals, fields, statics, array indexing, `get_stack`, and
watchpoint/exception reporting. A guard against accidentally mutating a production JVM, **not** a
security boundary: anyone who can reach the JDWP port can open their own connection without it.

## Compared with the other Java debugging MCP servers

Two other projects solve the same problem, and one of them is this repository's **parent**: this is a
fork of [`navicore/jdwp-mcp`](https://github.com/navicore/jdwp-mcp), which is where the JDWP client,
the 13 original tool names and the architecture diagram above come from. The other is
[`d4n-sec/jdb-mcp`](https://github.com/d4n-sec/jdb-mcp), an independent implementation built on JDI
instead of the wire protocol. All three are MIT. The rows below were read off each repository — code,
not just README — on **29 Jul 2026**; the two upstreams move, so re-check before quoting this.

| | **this fork** | `navicore/jdwp-mcp` | `d4n-sec/jdb-mcp` |
| --- | --- | --- | --- |
| Talks to the JVM through | JDWP, implemented natively (Rust) | JDWP, implemented natively (Rust) | JDI, the JDK's own debug API (Java) |
| Needs a JDK to run *the debugger* | no — one self-contained binary | no | **yes**, JDK 17+ (a JDK 7 build exists for legacy targets) |
| Debug tools | 33 | 13 | 13+ (11 listed, `tools/list` for the rest) |
| Stepping | yes | tool exists; the handler returns `"Step over not yet implemented"` | yes |
| Expression evaluation | chains, static and instance methods, overload resolution on runtime types, subscripts, deep expansion | same stub as stepping | not yet — `debug_calc` is the first item on its TODO list |
| Watchpoints / method-exit values | field read+write with old → new; return value **and which `return`** | no | `debug_set_watchpoint`, method entry/exit monitors |
| Conditional, hit-count and thread-filtered stop points | yes | no | on the TODO list |
| Wildcard / batched class patterns when arming | **yes** — one wildcard arms every matching loaded class *and* keeps arming ones that load later, addressable as one id; every arming tool also takes a list of patterns | no | on the TODO list (as "package prefix filtering" and "batch class filtering") |
| Non-suspending trace mode | yes, on breakpoints, exception stops and watchpoints, with measured per-hit cost | no | not documented |
| Hot reload / frame rewind | `RedefineClasses` + `PopFrames`, with all twelve HotSpot refusals mapped | no | no |
| Concurrent sessions | yes, with `debug.list_sessions` | single | single — explicitly, "eliminating the need for complex `sessionId` management" |
| Transports | stdio | stdio | stdio **and HTTP** |
| A hit reaches the agent by | `notifications/message` **push**, plus the buffered `debug.get_last_event` — push for *suspending* hits only, since a traced stop point is built to fire hundreds of times a second (EVT-2) | event loop is listed as a next step | **MCP notifications** |
| Launching the target for you | **yes** — `debug.launch`, and it holds the JVM *before its first instruction* (`suspend=y`), so a breakpoint in a static initialiser can fire | no | no (`debug_launch` is on its TODO list) |
| `METHOD_ENTRY` monitors across a class pattern | **no**, deliberately (METH-1) | no | `debug_set_method_entry` |
| Maintenance | this repo | GitHub copy **archived 28 Apr 2026**, moved to `git.navicore.tech` (last commit there 6 Oct 2025) | active, last push 30 Jan 2026 |

**What the fork actually changed.** Upstream's README still lists "event loop for async breakpoint
notifications", "stepping commands", "expression evaluation" and "string and object dereferencing" under
*Next Steps*, and its `handle_evaluate` / `handle_step_*` return their TODO strings — so upstream is a
working JDWP client and a set of tool names, and every capability in the Features list above except
attach, line breakpoints and `get_stack` was written here. The largest additions are the ones a shared,
long-running app server forces on you: an expression evaluator that resolves overloads the way `javac`
would, non-suspending trace mode, the watchdog and read-only mode, hot reload with `pop_frame`,
staleness detection, and a thread dump that names lock owners.

**Where the others are ahead.** `jdb-mcp` ships an **HTTP transport**; stdio is the only way into this
server. It also has `debug_set_method_entry`, a `METHOD_ENTRY` monitor across a class pattern, which
here is a deliberate omission rather than a gap — with a `ClassMatch` it is the noisiest event in JDWP,
and "what called this?" is answered far more cheaply by a traced breakpoint's caller chain (METH-1,
TRACE-5) — but if you want that firehose, it has one and this does not. Being JDI-based it also
inherits Oracle's implementation of the hard parts: a wire-protocol bug here is ours to find, and some
have been (the packet reader had to become its own task because `select!` was cancelling it mid-packet).
Upstream, for its part, is a much smaller codebase and still the clearest place to read how JDWP framing
works in Rust.

**Their roadmaps, measured against this.** Upstream's four *Next Steps* — event loop, stepping,
expression evaluation, string/object dereferencing — are all implemented here. `jdb-mcp`'s six TODO items
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

Which JDK it used is printed once per run and repeated as the last line, because a green run that cannot
be attributed to a version is worth less than it looks (TEST-18,
[#52](https://github.com/YgorPerez/java-debugging-mcp/issues/52)):

```
JDK in use: javac 11.0.30 at /home/you/.jdks/ms-11.0.30 (found via JAVA_HOME)
```

With `JAVA_HOME` unset the harness searches `PATH` and then a snap-installed IntelliJ's bundled runtime,
and the banner says which it settled on. Setting `JAVA_HOME` is a **request for that specific JDK**: if it
is not a usable one — a JRE with no `javac`, most often — the run fails and names what was missing rather
than quietly testing a different JVM, which is what it used to do.

`mcp-server/tests/stdio_protocol.rs` is one exception: it drives the real binary's JSON-RPC front door
with malformed input (unparseable lines, non-objects, missing `method`, EOF mid-message) and needs no JDK,
so it runs in plain `cargo test`. Each case checks that an error came back **and** that the server is
still serving afterwards, since one bad line from a client must not end the session.

The **cassette** tests are the other (see below). They live in `mcp_integration.rs` but carry no `#[ignore]`
— which means `scripts/integration-test.sh` does *not* run them, since `--ignored` runs only ignored tests.
Both commands are needed to see the whole file.

#### Recorded sessions: testing the debugger with no JVM at all

A third proxy mode **records** every JDWP request/reply pair to a file, and a replay server answers from
that file with nothing behind the port (ADR-0014, TEST-12
[#37](https://github.com/YgorPerez/java-debugging-mcp/issues/37)):

```bash
cargo test --test mcp_integration list_methods_renders_java_signatures_from_a_cassette   # no JDK needed
JDWP_RERECORD_CASSETTES=1 scripts/integration-test.sh a_recorded_session_replays          # re-record
```

The cassettes are in `mcp-server/tests/cassettes/` and are meant to be read and edited: JSON, one object
per exchange, payloads as hex in 32-byte lines, each exchange labelled with its JDWP command name. Answers
are keyed by `(command set, command, request payload)` rather than by arrival order, and **a request the
cassette cannot answer gets no reply at all** — the connection drops, the command is named on stderr, and
the test fails. A replay that quietly returned an error reply would make every test using it meaningless.

Two things this buys that a probe cannot:

- **One visit to a real instance becomes a permanent fixture.** Record once, replay forever, with no
  access, no JDK and no JVM.
- **Shapes nothing here can produce become testable by editing a file.**
  `method_exit_on_a_jdwp_1_5_vm.json` is a hand edit of a five-exchange recording that makes the debuggee
  answer `JDWP 1.5`, which reaches `debug.set_method_exit_stop`'s degraded arming — a branch a JDK matrix
  cannot reach, because JDWP's version tracks the JDK's and the oldest JVM in the estate speaks 1.11.

Events are **not** replayed: a composite event answers no request, so it has no key. The recorder counts
them and writes the count into the cassette, and says so when it is non-zero.

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

The round trip is a **dial** (`relay.set_rtt(rtt)`), not just a constructor argument, and a test that
*compares* two latencies should use it rather than standing up a second relay. Two relays mean two
attaches, which puts a JVM handshake and several seconds between the readings — long enough on a box
running the rest of this suite for a load spike to land on one of them and not the other, which is
indistinguishable from the wire. Turning the dial under one live connection, alternating the arms and
scoring each on its *fastest* sample, puts both readings in the same few seconds of the same machine
(TEST-13, [#38](https://github.com/YgorPerez/java-debugging-mcp/issues/38)).

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
scripts/doctor.sh --findings   # the findings the gate counts, and whether it would pass
scripts/doctor.sh --verbose    # per-finding file:line detail
scripts/doctor.sh --diff main  # only files changed vs main
```

**The score is not the gate.** CI fails the build on any warning, so a 100/100 "Great" can still be a
red build — v0.2.0's tag build was, on five `excessive-clone` findings that had been sitting in a local
run nobody could read out of it. `--findings` prints each warning/error in the same shape the CI step
summary uses, says whether the gate would pass, and exits 3 if it would not:

```
- **warning** `excessive-clone` — `src/handlers.rs:3233`
  `.clone()` inside a loop — may cause repeated heap allocations
```

It also names what the run did *not* look at — passes skipped for a missing tool, and passes that ran
only because you have a tool CI does not install — since either one moves the verdict away from CI's.

The same check runs in CI (`.github/workflows/rust-doctor.yml`, pinned to 0.2.0): it **gates on
warnings** — a finding fails the build (#18) — and uploads results to GitHub code scanning (SARIF).
Installing the optional external tools (`cargo install cargo-audit cargo-deny cargo-machete
cargo-geiger`) unlocks the dependency/unsafe passes it otherwise skips.

Because it gates on warnings, the Rust toolchain there is pinned, so a new pedantic lint in a future
clippy cannot break a build on code nobody touched. `.github/workflows/toolchain-pin.yml` runs the same
scan against `stable` once a month **without gating**, and opens an issue when the pin is behind — the
bump is scheduled work rather than a surprise. See ADR-0007.

One `clippy.toml`, at the workspace root, covers every crate; adding a workspace member needs nothing.
It only applies because `scripts/doctor.sh` and the workflows set `CLIPPY_CONF_DIR` — rust-doctor drops
a temporary `clippy.toml` into any member that lacks one, which would otherwise shadow it. The file
says the rest.

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

✅ **Functionally complete** — 33 debug tools, integrated and validated against a live JVM.

### Implemented
- [x] JDWP protocol (handshake, packets, encoding/decoding)
- [x] MCP server with 33 debug tools (stdio transport)
- [x] VirtualMachine commands (Version, IDSizes, AllThreads, Suspend/Resume, CreateString, Capabilities/**CapabilitiesNew**, **RedefineClasses**)
- [x] ClassesBySignature, ReferenceType.Methods/Fields/Signature, ClassType.Superclass
- [x] Method.LineTable / VariableTable
- [x] EventRequest.Set/Clear/ClearAllBreakpoints — breakpoints with location, **count**, **thread**, **exception**, and **field** modifiers
- [x] ThreadReference.Frames, StackFrame.GetValues/SetValues/ThisObject/**PopFrames**
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
- [x] **Hot reload** — `RedefineClasses` from a class root, with the twelve refusals mapped onto what to
      do next, type-cache invalidation on success, and `PopFrames` so a suspended frame re-enters the new
      code (SWAP-1)
- [x] **Staleness detection** — per-method line tables from the JVM against a parsed `.class`, so "the
      deployed bytecode is older than your build" stops looking like a wrong hypothesis (DISC-7)
- [x] **Safety**: `panic` + idle watchdog auto-resume (clears breakpoints, exception breakpoints, watchpoints and method-exit requests; names any class left hot-reloaded, which it cannot undo)
- [x] **Performance**: type cache, `package_filter`, single-threaded `invoke_method`, token-trimmed output
- [x] Architecture independence (big-endian protocol; Intel & ARM)

## References

- [JDWP Specification](https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html)
- [Model Context Protocol](https://modelcontextprotocol.io/)
- [Claude Code MCP Documentation](https://docs.claude.com/claude-code)

## License

MIT
