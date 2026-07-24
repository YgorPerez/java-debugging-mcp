# Debugging a missing metric

**"Why isn't my custom metric showing up in `/actuator/metrics`?"**

This was the roadmap's headline use case (`docs/VARIABLE_INSPECTION_PLAN.md`). The question is really
*"is the metric registered, and under what name?"* — which is a question about live objects, so the
debugger answers it directly instead of you adding logging and redeploying.

## What's verified here, and what isn't

Every output block below is **captured from a real run**, not written by hand — but against
`probes/MetricsProbe.java`, a stand-in that reproduces Micrometer's object *shape*
(`meterRegistry.meters : Map<String, Counter>`, `Counter.id.name`, a real `AtomicInteger`) without
pulling Spring into the test harness. The integration test
`roadmap_metrics_inspection_criteria` asserts each of the roadmap's original success criteria against
it, so **the tool behaviour is proven**.

What a stand-in cannot prove is Spring's own specifics: real class names, line numbers, and the bean
lifecycle. Adapt those against your app — the commands and the shape of the answers carry over
unchanged.

Run the stand-in yourself:

```bash
cargo build --release
cd examples/probes && javac -g MetricsProbe.java
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8813 -cp . MetricsProbe &
```

## 1. Attach and break where the metric is used

```
debug.attach {host:"localhost", port:8813}
debug.set_breakpoint {class_pattern:"MetricsProbe$HelloController", method:"hello"}
```

For a real app, break in the controller method that touches the counter (or in the `@Bean` method that
registers it, if you suspect registration never happens).

## 2. Find out which thread stopped

```
debug.get_last_event
```

```
[event] {"class":"MetricsProbe$HelloController","event":"breakpoint","line":78,"method":"hello","thread":"0x1"}
[suspended] true
```

The event names the thread, so there's no hunting through a Spring app's hundreds of threads. Every
other tool defaults to this thread.

## 3. Read the registry in one call

This is the question. `expand_objects` walks the registry into its map and each map value into its
fields:

```
debug.evaluate {expression:"this.meterRegistry", expand_objects:true, max_depth:3}
```

```
this.meterRegistry = MetricsProbe$SimpleMeterRegistry (id=0x5) {
  meters = java.util.LinkedHashMap(3 entries) {
    "hello_requests_total" → MetricsProbe$Counter (id=0x16) {
      id = MetricsProbe$MeterId "hello_requests_total[uri=/hello]"
      count = (double) 42
    }
    "hello_errors_total" → MetricsProbe$Counter (id=0x1d) {
      id = MetricsProbe$MeterId "hello_errors_total[uri=/hello]"
      count = (double) 3
    }
    "http.server.requests" → MetricsProbe$Counter (id=0x21) {
      id = MetricsProbe$MeterId "http.server.requests[status=200]"
      count = (double) 5
    }
  }
}
```

That answers it outright: the metric **is** registered, and you can see the exact key it went in under.
A metric "missing" from `/actuator/metrics` is very often present here under a name you didn't expect —
a typo, or tags that split it into series you weren't querying.

Raise `max_depth` to expand `id` into `name` + `tags` too; `max_children` (default 16) bounds how many
map entries and fields are shown, and the output says when it truncated.

## 4. Narrow a large registry

A real registry has hundreds of meters. Filter instead of dumping — in a predicate, the left side
resolves against each element:

```
debug.evaluate {expression:"this.meterRegistry.meters.values()[?id.name != \"http.server.requests\"]"}
```

```
this.meterRegistry.meters.values()[?id.name != "http.server.requests"] = java.util.LinkedHashMap$LinkedValues[?id.name != "http.server.requests"] → 2 of 3 matched {
  [0] = MetricsProbe$Counter "Counter(hello_requests_total=42.0)"
  [1] = MetricsProbe$Counter "Counter(hello_errors_total=3.0)"
}
```

`2 of 3 matched` is the useful part: it tells you the filter ran over the whole collection, so `0 of N`
means genuinely no match rather than nothing scanned.

## 5. Pick out a single value

Once you know the key, go straight at it — a map subscript, then a field path:

```
debug.evaluate {expression:"this.meterRegistry.meters[\"hello_errors_total\"].count"}
debug.evaluate {expression:"this.helloCounter.id.name"}
```

```
this.meterRegistry.meters["hello_errors_total"].count = (double) 3
this.helloCounter.id.name = "hello_requests_total"
```

## 6. Clean up

```
debug.panic
```

Clears every breakpoint and resumes all threads. Do this before you walk away: a suspended thread in a
Spring app means a stuck request.

## If the metric is genuinely absent

The registry not containing it moves the question upstream to registration. Two tools for that:

- **Break where registration should happen** and check whether the code runs at all. If it's a `@Bean`
  or `@PostConstruct` that may not have executed, set the breakpoint before triggering anything — a
  breakpoint on a not-yet-loaded class **defers** and arms itself when the class loads, so you don't
  have to race the startup.
- **Watch the registry field** to catch whoever replaces it:
  `debug.set_watchpoint {class_name:"…HelloController", field_name:"meterRegistry"}` reports the
  mutating `class.method:line` with the old → new value. Clear it when done — a watched field can't be
  JIT-optimised.

On a shared JVM, prefer `trace:true` logpoints over suspending breakpoints: they snapshot the frame and
resume immediately, so a forgotten breakpoint can't freeze other people's requests. Read them with
`debug.get_traces`.
