// Probe for SWAP-1 (hot reload — VirtualMachine.RedefineClasses), driven by mcp_integration.rs.
//
//   javac -g SwapProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8814 -cp . SwapProbe
//
// What a hot-reload test needs from a debuggee is narrow and unusual: a value the probe PRINTS, coming
// from a method body a test can rewrite, in a class the JVM has already loaded. Everything else here
// exists to make the failure modes distinguishable.
//
// The call chain (main → tick → answer) is deliberate rather than decorative. A breakpoint inside
// `answer` leaves a frame that is neither the outermost nor a native one, which is the only shape
// `debug.pop_frame` can actually pop — and popping it is the whole of SWAP-1's fourth piece, since a
// frame already on the stack keeps running the bytecode it entered with. With everything inlined into
// `main` the pop would be refused (NO_MORE_FRAMES) and the interesting half would be untestable.
//
// The tick counter comes from `main`, which the test never rewrites, so a probe that kept running is
// distinguishable from one that froze — "the swap did nothing" and "the JVM never resumed" are
// different bugs and the output has to tell them apart.
public class SwapProbe {

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            tick(i);
            Thread.sleep(150);
        }
    }

    /** One line of output per loop. Not rewritten by any test — it is the control. */
    static void tick(int i) {
        System.out.println("answer " + answer() + " tick " + i);
    }

    /**
     * The method every swap test rewrites, by replacing the marked line's literal. A local rather than
     * a `return 1;` so the method has a line worth stopping on: a breakpoint here suspends INSIDE the
     * method being redefined, which is the case the whole feature is awkward in.
     */
    static int answer() {
        int v = 1; // SWAP_VALUE
        return v;
    }
}
