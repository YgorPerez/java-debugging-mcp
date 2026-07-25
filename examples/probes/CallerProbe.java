// Probe for TRACE-5 (caller frames on trace snapshots), driven by mcp_integration.rs.
//
//   javac -g CallerProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8817 -cp . CallerProbe
//
// The point of this probe is that ONE traced location is reached from TWO different callers, which is
// the only shape that can tell a real caller chain from a hardcoded frame:
//
//   main -> alpha -> record(1)              reached from alpha
//   main -> beta  -> record(2)              reached from beta
//   main -> beta  -> nested -> record(3)    three deep, so depth > 1 has something to show
//
// The traced line itself is marked further down; markers are matched by substring, so this header
// deliberately does not spell one out.
//
// A single-caller probe would let a snapshot that always reports the same frame pass. The `v` argument
// identifies which path a given hit came from, so a test can pair each caller chain with its hit
// instead of assuming an order.
//
// `record` deliberately does nothing but assign: the traced line must not be optimised away, and the
// snapshot's payload should be the argument, not a side effect.
//
// The tick line is load-bearing, as in ExcProbe: a traced logpoint must let the probe keep printing,
// which is what actually proves no thread was left suspended. A suspending one stops the ticks dead,
// and the debugger reports success either way.
public class CallerProbe {

    static int calls = 0;
    static int lastValue = -1;

    // The traced location. Reached from three distinct call paths.
    static void record(int v) {
        calls++;
        lastValue = v; // BP1
    }

    static void alpha() {
        record(1);
    }

    static void beta(boolean deeper) {
        if (deeper) {
            nested();
        } else {
            record(2);
        }
    }

    // One more level, so `trace_frames: 2` sees beta above it and `main` above that.
    static void nested() {
        record(3);
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            alpha();
            beta(false);
            beta(true);
            System.out.println("tick " + i + " calls=" + calls + " last=" + lastValue); // BP2
            Thread.sleep(150);
        }
    }
}
