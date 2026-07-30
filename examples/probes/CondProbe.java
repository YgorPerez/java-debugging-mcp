// Probe for FILT-7 (#91) — a `condition` on a SUSPENDING stop point must not freeze the whole VM on
// every hit, only on the hit where the condition turns out to be true.
//
//   javac -g CondProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8797 -cp . CondProbe
//
// Two threads that share nothing but a sink field:
//
//   "ticker"  burns a fixed chunk of arithmetic, prints `tick <n>`, repeats. It is the witness, and it
//             is CPU-bound ON PURPOSE. A ticker that slept between ticks would be a bad witness here: a
//             sleeper needs microseconds of CPU per tick and would keep ticking at close to its full
//             rate through a VM that is suspended 95% of the wall clock, because it only ever needs one
//             of the short windows between two freezes. A thread that has to EARN each tick reports the
//             running fraction directly, which is the quantity in question.
//
//   main      calls hot(n) with n counting up, printing `work <n>` per call. The conditioned line lives
//             in hot(), and the condition a test arms (`n == <a value>`) is false on every hit but one.
//
// The reading that matters is the RATIO — ticks per `work` line. Nothing else distinguishes "the
// debugger evaluated the condition on the hit thread" from "the debugger froze the world to evaluate the
// condition": both leave the debuggee computing the right answer, and both make every tool report
// success. The debugger's own reply is not evidence, which is the whole reason this probe exists.
//
// main waits for a `go` line on stdin before it starts hitting: unbreakpointed and unthrottled the loop
// would print millions of lines before a test could arm anything.
import java.io.BufferedReader;
import java.io.InputStreamReader;

public class CondProbe {

    // Arithmetic per tick. Sized so a tick costs a millisecond or so of CPU rather than microseconds —
    // see the note above about why the witness must be CPU-bound.
    static final int WORK_PER_TICK = 3_000_000;

    // Written by both threads so neither loop can be optimised away entirely. Deliberately not
    // synchronised: the value is never read back and a lock would couple the two threads, which would
    // make the ticker's rate say something about the worker rather than about the VM.
    static volatile long sink;

    // The conditioned method. `n` is a PARAMETER, so it is in the local-variable table from the first
    // instruction and a condition can read it at BP1 without needing a statement to have run first.
    static void hot(int n) {
        sink += n; // BP1
    }

    public static void main(String[] args) throws Exception {
        Thread ticker = new Thread(() -> {
            long acc = 0;
            for (int t = 0; t < 2_000_000; t++) {
                for (int i = 0; i < WORK_PER_TICK; i++) {
                    acc += i ^ t;
                }
                sink += acc;
                System.out.println("tick " + t);
            }
        }, "ticker");
        // Daemon so a killed test leaves nothing spinning, and bounded above so a probe whose harness
        // died cannot burn a core forever.
        ticker.setDaemon(true);
        ticker.start();

        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));
        String cue;
        while ((cue = in.readLine()) != null && !cue.trim().equals("go")) {
            // keep waiting
        }

        for (int n = 0; n < 1_000_000; n++) {
            hot(n);
            System.out.println("work " + n);
        }
    }
}
