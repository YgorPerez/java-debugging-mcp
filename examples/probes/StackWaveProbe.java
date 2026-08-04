// Probe for PERF-1 (#100): a deep stack whose frames hold locals and nothing else.
//
//   javac -g StackWaveProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8821 -cp . StackWaveProbe
//
// `debug.get_stack --include_variables` makes three reads per frame — the method's line table, the
// method's variable table, and the frame's values — and used to make all of them one at a time. This probe
// is shaped to measure exactly that and nothing else, which took three deliberate choices:
//
//   DEPTH = 60          The low end of a servlet request stack, as `PoolShapeProbe` argues at length.
//
//   ONE recursive method, where `PoolShapeProbe` uses 60 distinct ones. The opposite choice, for the
//                       opposite reason: that probe needs distinct methods so a per-(class, method) cache
//                       cannot look perfect, and this one needs a single method so the DEDUPLICATION is
//                       measurable — 60 frames naming one method should cost one line table and one
//                       variable table, and before #100 they cost sixty of each.
//
//   PRIMITIVE locals only. This is the load-bearing one. A `String` local costs a
//                       `StringReference.Value` round trip to render and an object local costs an
//                       `ObjectReference.ReferenceType`, per local, per frame — so a probe with one
//                       `String` in scope would be measuring the renderer (PERF-2, #129) rather than the
//                       three reads under test. An `int` and a `long` render from bytes already in hand.
//
// The bottom frame parks on a monitor rather than spinning, so the debugger's own timings are not
// competing with the debuggee for cores. `main` stays out of the chain and prints the tick line, which is
// the only evidence the VM was resumed.
public class StackWaveProbe {

    /// Frames in the chain below `main`. Low end of a real request stack.
    static final int DEPTH = 60;

    static final Object GATE = new Object();

    /// The bottom of the chain: park forever, holding every frame above.
    ///
    /// A stop point on the `wait` line suspends the thread with the whole chain on the stack, which is what
    /// a stack walk needs — `get_stack` reads frames and a running thread has none to read.
    ///
    /// **The wait is timed and looped, and the first version was neither.** An untimed `wait()` is reached
    /// once, within milliseconds of the probe starting and long before a debugger has attached and armed
    /// anything, so the line never executes again and the stop point never fires: the test failed with 126
    /// ticks of output and a thread parked exactly where it was wanted. Re-entering the line every 200ms
    /// costs nothing — the thread is still parked, it just becomes reachable — and it is what makes the
    /// suspension happen when the debugger asks rather than before it arrives.
    static void park() {
        synchronized (GATE) {
            while (true) {
                try {
                    GATE.wait(200); // suspended here for the life of the probe
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                }
            }
        }
    }

    /// One frame of the chain, holding three locals in scope at the recursive call.
    ///
    /// All three are primitives on purpose (see the header). `depth` is the parameter, and `doubled` and
    /// `widened` are assigned before the call so they are genuinely live at the bytecode index the walk
    /// asks about — a local declared after the call would be out of scope there and would make the
    /// variable-table read look cheaper than it is.
    static void down(int depth) {
        int doubled = depth * 2;
        long widened = depth;
        if (depth <= 0) {
            park();
            return;
        }
        down(depth - 1);
        // Read after the call so the compiler cannot narrow their scope to before it.
        if (doubled < 0 || widened < 0) {
            System.out.println("unreachable " + doubled + widened);
        }
    }

    public static void main(String[] args) throws Exception {
        Thread worker = new Thread(() -> down(DEPTH), "stack-wave-worker");
        worker.setDaemon(true);
        worker.start();
        // The tick line is the readiness signal AND the resume evidence, as in every other probe here.
        for (int i = 0; ; i++) {
            System.out.println("tick " + i);
            System.out.flush();
            Thread.sleep(200);
        }
    }
}
