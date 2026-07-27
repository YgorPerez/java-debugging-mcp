// Probe for TEST-10 (#35): a stack containing the three constructs the compiler and the JVM invent
// classes for — a lambda, a method reference, and an anonymous inner class.
//
//   javac -g SyntheticProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8825 -cp . SyntheticProbe
//
// Not one of the seventeen probes before this one put a synthetic frame in a stack a test read, so
// `decode_signature` had only ever been handed names a human wrote. A dump of any real application
// server is full of the other kind, and the failure mode is not a crash — it is a class name that comes
// back subtly wrong and reads as plausible, which is the sort of thing a caller acts on before noticing.
//
// The three kinds are genuinely different things, and the debugger sees them differently:
//
//   ANONYMOUS INNER CLASS  A real class file with a real name, `SyntheticProbe$1`. Ordinary in every
//                          way except that the name says nothing about what it does — so the frame is
//                          only actionable if it also carries a SOURCE LINE, which it does.
//   LAMBDA                 Two frames, not one. The body is compiled to a synthetic static method on the
//                          enclosing class (`lambda$…`), and it is reached through an instance of a JVM
//                          **hidden class** whose name contains a `/` and an address —
//                          `SyntheticProbe$$Lambda/0x00007f…`. The `/` is not a package separator.
//   METHOD REFERENCE       The same hidden-class frame, but with no `lambda$…` body underneath: the
//                          generated `run` calls the referenced method directly.
//
// All three are in ONE thread's stack rather than three threads', because that is how it happens: a
// handler passes a lambda to a framework, which passes it through a listener, and the frames end up
// interleaved with ordinary ones. `call(Runnable)` sits between every pair so no synthetic frame is
// adjacent to another and each can be identified by what is above and below it.
//
// The worker parks at the bottom and never returns, so the stack is stable for as long as the test wants
// it. main polls the worker's own `getState()` for WAITING rather than having it announce itself: there
// is no code left to run after it parks, so anything it printed first would only mean "about to park".
public class SyntheticProbe {

    static final Object GATE = new Object();

    /// The bottom of the stack: parked forever, holding every frame above it.
    static void park() {
        synchronized (GATE) {
            try {
                GATE.wait();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    /// The target of the METHOD REFERENCE below. A plain static method — the interesting frame is the
    /// generated one that calls it.
    static void viaMethodReference() {
        park();
    }

    /// The ordinary frame every synthetic one is reached through, so the stack alternates
    /// invented/real/invented and no two generated frames touch.
    static void call(Runnable r) {
        r.run();
    }

    /// The LAMBDA. Its body becomes a synthetic `lambda$lambdaStep$0` on this class; the object handed
    /// back is an instance of a hidden class the JVM names at run time.
    static Runnable lambdaStep() {
        return () -> call(SyntheticProbe::viaMethodReference);
    }

    /// The ANONYMOUS INNER CLASS. Declared first and the only one in the file, so it is `SyntheticProbe$1`
    /// and a test can say so without counting.
    static Runnable anonymousStep() {
        return new Runnable() {
            @Override
            public void run() {
                call(lambdaStep());
            }
        };
    }

    static void worker() {
        call(anonymousStep());
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(SyntheticProbe::worker, "synthetic-worker");
        t.setDaemon(true);
        t.start();

        // Parked means the whole chain is on the stack. Asking the JVM removes the race that any
        // announcement from inside the worker would leave behind.
        while (t.getState() != Thread.State.WAITING) {
            Thread.sleep(20);
        }
        System.out.println("parked");

        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i); // BP1
            Thread.sleep(150);
        }
    }
}
