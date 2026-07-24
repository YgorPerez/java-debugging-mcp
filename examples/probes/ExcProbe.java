// Probe for TRACE-2 (non-suspending exception breakpoints), driven by mcp_integration.rs.
//
//   javac -g ExcProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8815 -cp . ExcProbe
//
// Reproduces the failure shape the whole project exists for: an exception thrown and then SWALLOWED
// by a catch block that records nothing anyone upstream can see. The caller gets a plausible-looking
// answer, so no log line and no stack trace ever appears — the only way to see it is to break (or
// better, trace) on the throw itself.
//
//   integrate(i)  throws SwallowedException, catches it, sets lastStatus and returns normally
//   main()        prints a "tick" line every iteration
//
// The tick line is the load-bearing part of the test: a traced exception breakpoint must let the
// probe keep printing, which is what actually proves no thread was left suspended. A suspending one
// stops the ticks dead, and the absence of a complaint from the debugger would not have told us that.
public class ExcProbe {

    // A custom type, not a JDK one: the exception breakpoint pins itself to this class's ref type, so
    // an internal JVM throw can't be mistaken for a hit.
    static class SwallowedException extends RuntimeException {
        SwallowedException(String message) {
            super(message);
        }
    }

    static int attempts = 0;
    static String lastStatus = "none";

    // The swallow. `lastStatus` is the only trace it leaves, and nothing reads it — exactly like a
    // catch block whose save() is commented out.
    static void integrate(int i) {
        attempts++;
        try {
            throw new SwallowedException("integration failed on " + i); // BP1
        } catch (SwallowedException e) {
            lastStatus = "swallowed:" + i;
        }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            integrate(i);
            System.out.println("tick " + i + " attempts=" + attempts + " " + lastStatus); // BP2
            Thread.sleep(150);
        }
    }
}
