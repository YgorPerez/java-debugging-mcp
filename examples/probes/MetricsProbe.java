// Probe for the original roadmap's headline use case: "why isn't my custom metric showing up in
// /actuator/metrics?" (appendix items 10 and 14).
//
//   javac -g MetricsProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8813 -cp . MetricsProbe
//
// This is a STAND-IN for Spring Boot + Micrometer, which can't be a test dependency here (it would
// mean pulling a Maven world into the harness). It reproduces the exact object *shape* the roadmap
// wanted to inspect, so the tool is verified against the real structure even though the framework is
// simulated:
//
//   controller.meterRegistry.meters : Map<String, Counter>   ← the collection to drill into
//   Counter.id.name / .tags         : nested object + List    ← the field path that must resolve
//   Counter.count                   : double                  ← the value that must read as 42.0
//   controller.requestCount         : AtomicInteger            ← a real JDK library object
//
// The gap a stand-in cannot close: Spring's own class names, line numbers, and bean lifecycle. Those
// have to be adapted against a live app; see examples/observability-debugging.md.
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;

public class MetricsProbe {

    static class MeterId {
        String name;
        List<String> tags = new ArrayList<>();

        MeterId(String name, String tag) {
            this.name = name;
            this.tags.add(tag);
        }

        @Override public String toString() { return name + tags; }
    }

    static class Counter {
        MeterId id;
        double count;

        Counter(String name, String tag, double count) {
            this.id = new MeterId(name, tag);
            this.count = count;
        }

        @Override public String toString() { return "Counter(" + id.name + "=" + count + ")"; }
    }

    static class SimpleMeterRegistry {
        // The collection the roadmap's success criteria drill into.
        Map<String, Counter> meters = new LinkedHashMap<>();

        void register(Counter c) { meters.put(c.id.name, c); }
    }

    static class HelloController {
        SimpleMeterRegistry meterRegistry = new SimpleMeterRegistry();
        Counter helloCounter;
        // A genuine JDK library object, so field expansion is exercised against code we don't own.
        AtomicInteger requestCount = new AtomicInteger(42);

        HelloController() {
            helloCounter = new Counter("hello_requests_total", "uri=/hello", 42.0);
            meterRegistry.register(helloCounter);
            meterRegistry.register(new Counter("hello_errors_total", "uri=/hello", 3.0));
            // A metric registered under a name nobody queries — the usual reason one "goes missing".
            meterRegistry.register(new Counter("http.server.requests", "status=200", 5.0));
        }

        // helloCounter.count is deliberately NOT incremented here: a test attaches at an arbitrary
        // iteration, so any value this method mutates is non-deterministic. The roadmap criterion is
        // "read count=42.0 as a number rather than an object id", which needs the number to hold still.
        // requestCount is the one that moves, and nothing asserts its exact value.
        String hello() {
            int n = requestCount.incrementAndGet();
            return "hello " + n; // BP1: `this` is the controller, so this.meterRegistry.meters is in reach
        }
    }

    public static void main(String[] args) throws Exception {
        HelloController controller = new HelloController();
        for (int i = 0; i < 100000; i++) {
            System.out.println(controller.hello());
            Thread.sleep(150);
        }
    }
}
