// Probe for EVAL-5 (a rendering invocation must be bounded and reported), driven by mcp_integration.rs.
//
//   javac -g SlowToStringProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8822 -cp . SlowToStringProbe
//
// Rendering a value calls its `toString()` in the debuggee. Against a real WildFly, `toString()` on an
// Undertow request object never returned: `INVOKE_SINGLE_THREADED` runs only the target thread, so a method
// needing a monitor held by one of the other (still suspended) threads cannot finish. The tool waited for
// the event loop's generic 30s reply timeout — swept every 10s, so 30-40s of frozen VM — and then rendered
// the value shallowly, identically to the free path. Invisible.
//
// Two locals at the breakpoint, so one call can show both halves of the fix:
//
//   slow  — toString() sleeps far longer than any sane budget. Stands in for "never returns". Sleeping is
//           deterministic where a real deadlock is not, which is the point of a probe.
//   fast  — toString() answers immediately, proving the budget does not punish ordinary values.
//
// `Blocker.toString()` sleeps rather than taking a lock on purpose. A genuine monitor deadlock would leave
// a thread wedged for the rest of the JVM's life, which makes the test's own cleanup unreliable; a sleep
// reproduces the *observable* behaviour — an invocation that outlives its budget — and then goes away.
public class SlowToStringProbe {

    /** Longer than any plausible invocation budget, short enough that the JVM tidies up after the test. */
    static final long TOSTRING_MS = 20000;

    static class Blocker {
        int id = 7;

        @Override public String toString() {
            try {
                Thread.sleep(TOSTRING_MS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            return "Blocker(" + id + ") finally!";
        }
    }

    static class Quick {
        int id = 42;

        @Override public String toString() {
            return "Quick(" + id + ")";
        }
    }

    static void inspect(Blocker slow, Quick fast, int n) {
        int local = n;
        System.out.println("inspect " + local + " " + slow.id + " " + fast.id); // BP1
    }

    public static void main(String[] args) throws Exception {
        Blocker slow = new Blocker();
        Quick fast = new Quick();
        for (int i = 0; i < 100000; i++) {
            inspect(slow, fast, i);
            System.out.println("tick " + i); // BP2
            Thread.sleep(150);
        }
    }
}
