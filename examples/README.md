# Examples

Worked debugging scenarios, and the Java programs everything here attaches to.

For the full tool surface see [`docs/tools.md`](../docs/tools.md); for build and test instructions,
[`docs/development.md`](../docs/development.md).

## Worked scenario

### [observability-debugging.md](observability-debugging.md)

**"Why isn't my custom metric showing up in `/actuator/metrics`?"** — answered by reading the live
registry instead of adding logging and redeploying. Every output block is captured from a real run
against `probes/MetricsProbe.java`, a stand-in for Spring + Micrometer; a closing section records what
actually differed when the same criteria were run against a **real Spring Boot 2.6 app with
`micrometer-registry-prometheus`** (84 live meters), which is the part a stand-in cannot prove.

What it demonstrates: finding the thread that stopped, `expand_objects` on a nested registry, predicate
filters over a large collection, map subscripts, `package_filter` on a reflection-heavy stack, and what
to do when the metric is genuinely absent.

## Where to start, by question

| You want to | Start with |
| --- | --- |
| See what a running JVM actually holds | `debug.get_stack`, then `debug.evaluate {expand_objects:true}` |
| Find the class name to arm a stop point on | `debug.list_classes {filter:"com.example.*"}` — the loaded truth, including generated proxies |
| Observe a **shared** instance | `trace:true` on the stop point, then `debug.get_traces` — never a bare breakpoint |
| Read one live worker without stopping the VM | `debug.suspend_thread`, then `debug.resume_thread` |
| Find out why a chain returned null | `debug.evaluate_chain` |
| Find out what a method returned | `debug.set_method_exit_stop` |
| Find out who wrote a field | `debug.set_field_stop` |
| Find out why requests are hanging | `debug.thread_dump` (suspending) or `debug.set_monitor_stop` (live) |
| Check the JVM is running your build | `debug.check_stale` |
| Get out of trouble | `debug.panic` |

## Prompts that work

The server is driven in natural language; these are the shapes that map cleanly onto tools.

**Connect and orient**
```
Attach to the JVM at localhost:5005
What classes matching com.example.* are loaded?
List the methods of com.example.OrderService
```

**Stop points**
```
Set a breakpoint at com.example.MyClass line 42
Break on the first line of OrderService.submit
Break only when qty > 3 and status is not "OK"
Break on the 5th hit only
List the active stop points          # also shows hit counts and what each trace is costing
Clear bp_1
```

**Observe without freezing anything** — the default posture on a shared JVM:
```
Trace com.example.OrderService.submit without suspending, capture 5 caller frames
Trace every throw of InfoTravelException without suspending
Show me the traces
```

**Inspect**
```
Show me the current stack with variables
Evaluate this.registry.getMeters()[?getId().getName() == "jvm.memory.used"]
Expand order.customer to depth 3
Which link in order.getCustomer().getAddress().getCity() went null?
Read dsRequest as ISO-8859-1 text
```

**Control**
```
Continue
Step over / step into / step out
Suspend the thread named "http-nio-8080-exec-3"
Resume it
```

**Clean up** — do this before you walk away; a suspended thread in an app server is a stuck request:
```
Panic
Disconnect
```

## probes/

The Java programs everything attaches to. The integration harness compiles and launches these for you;
only the `jdwp-client` examples below need you to do it by hand:

```bash
javac -g Probe.java   # -g is required: no -g means no local-variable table, so no locals at all
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005 -cp . Probe
```

Use a fresh port per run — `server=y` stops listening after the first connection. Breakpoint lines are
marked with `// BP<n>` comments so tests can find them by marker instead of by number.

## test_*.rs — raw protocol examples

These drive `jdwp-client` directly, below the MCP layer, and are for library development: connection and
handshake (`test_connection`), VirtualMachine commands (`test_vm_commands`), class and method lookup
(`test_find_class`), arming breakpoints (`test_breakpoint`, `test_deferred_bp`, `test_exception_bp`,
`test_trace_bp`), stack and variable reads (`test_manual_stack`, `test_stack_inspection`,
`test_string_values`, `test_static_field`), and field writes (`test_set_field`). Each needs a probe JVM
you started yourself, on the port it names.

**MCP-level coverage is not here.** Tests that exercise the *server's handlers* live in
`mcp-server/tests/mcp_integration.rs` and drive the real `jdwp-mcp` binary over JSON-RPC against probe
JVMs they launch and reap themselves:

```bash
scripts/integration-test.sh                # all of them
scripts/integration-test.sh force_return   # filter by name
```

Needs a JDK — without one each test prints `SKIP` and passes, so check for `SKIP` before trusting a green
run. See [`docs/development.md`](../docs/development.md) for the JDK matrix, sharding and the cassette
tests that need no JVM at all.

## Remote targets

```bash
kubectl port-forward pod/my-app-pod 5005:5005
```

Then attach to `localhost:5005`. A port-forwarded JVM is usually a **shared** one, so prefer `trace:true`
and `debug.suspend_thread` over anything that suspends the VM, and set `JDWP_READONLY=1` if you only mean
to look.

## Troubleshooting

**Connection refused.** The app needs `-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005`,
and `address=*:` rather than a bare port — a JDK 9+ JVM binds to localhost only otherwise, which a
port-forward cannot reach.

**The breakpoint never fires.** `debug.list_stop_points` reports `Hits: 0` explicitly rather than staying
silent, so "armed and never fired" is a statement you can trust. Then: is the class loaded
(`debug.list_classes`) and is it the class you think — a Quarkus `Foo_Subclass` extends *your* `Foo`, and
a WildFly deployment can hold two copies of one name from different classloaders. Is the line the one you
compiled (`debug.check_stale`)? Is the deployed bytecode your build at all (`debug.source`)?

**Variables read as `Type @0x…` instead of values.** Two different causes, and they need opposite fixes:

- **Read-only session.** Pretty-printing an object means invoking `toString()` in the debuggee, which
  `read_only` refuses. That `@0x…` is a handle `debug.evaluate` accepts as an expression head, so it is
  somewhere to go rather than a dead end.
- **Expansion is opt-in.** Pass `expand_objects:true` for a field tree (ADR-0006). It invokes nothing —
  fields only — so it works on a read-only session and on a thread held by `debug.suspend_thread`.

**"Cannot invoke" / `INVALID_THREAD`.** JDWP allows a method invocation **only on a thread suspended by an
event**. So a getter, a `Map` subscript on a non-structural implementation, or a `toString()` render needs
a thread stopped at a breakpoint, watchpoint or exception stop.

Neither `debug.pause` nor `debug.suspend_thread` unlocks invocation — that is measured, and it is the one
piece of folk wisdom worth unlearning here, because "pause all threads first" sounds like it should work
and costs you the VM for nothing. What those two *do* unlock is everything that needs no invocation: the
stack with locals, field and static chains, `expand_objects`, array indexing, `set_value` on a local, and
structural collection reads on `HashMap` / `LinkedHashMap` / `ConcurrentHashMap` / `ArrayList`.

**Something is frozen.** `debug.panic` clears every stop point, resumes the VM and releases every thread
this session is holding, verifying each against the JVM's own suspend count. The watchdog would have done
it for you within `JDWP_WATCHDOG_SECS` (default 120).

## Contributing an example

1. Add a `.md` file here describing the scenario: the problem, the steps, the answer.
2. **Capture real output.** Every block in `observability-debugging.md` came from a run, and the doc says
   which parts a reader must adapt. Hand-written output ages into fiction.
3. If it needs a new probe, put it in `probes/` with `// BP<n>` markers.
4. Link it from the list at the top of this file.

## Resources

- [JDWP Specification](https://docs.oracle.com/javase/8/docs/technotes/guides/jpda/jdwp-spec.html)
- [Micrometer Documentation](https://micrometer.io/docs)
