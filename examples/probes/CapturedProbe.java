// Probe for TRACE-10 (an anonymous class's captured locals, and an object handle that outlives the
// snapshot it came from), driven by mcp_integration.rs.
//
//   javac -g CapturedProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8830 -cp . CapturedProbe
//
// The shape being reproduced is `infotravel`'s supplier fan-out: work is handed to a pool through an
// ANONYMOUS Callable, and everything the submitter knew — which session, which supplier, which attempt
// — crosses the thread boundary inside that object rather than as arguments. `javac` compiles those
// captures to synthetic `val$…` fields plus a `this$0` back-reference to the enclosing instance, and
// puts NONE of them in `call()`'s local variable table, so a snapshot inside the worker used to show a
// single `this` and nothing about the request that queued it.
//
// Deliberately not a lambda. A lambda body is desugared onto the *enclosing* class as
// `lambda$<method>$<N>`, so its captures arrive as ordinary parameters and were already visible; the
// anonymous class is the case that was not.
//
// Three things the probe provides that the test cannot arrange from outside:
//
//   1. `PINNED` — reachable from a `static final` for the JVM's whole life, so a handle taken from a
//      snapshot of it is guaranteed to still dereference minutes later.
//   2. `doomed` + the `drop` cue — the opposite, on demand: the reference is cleared and a collection
//      forced, so a handle a snapshot retained becomes VANISHED while the session is still open. That
//      is the ordinary case on a real pool and the one worth testing deliberately.
//   3. `toStringCalls` — a counter incremented by `Request.toString()` and by nothing else. Reading it
//      afterwards is how a test proves the capture invoked no method in the debuggee, rather than
//      asserting that on the strength of the code having been written carefully.
//
// `plain()` is the control: an ordinary static method on an ordinary class, traced in the same run, so
// "the captured section is absent here" is measured against a live site rather than against silence.
//
// The tick line is load-bearing, as in CallerProbe: a traced stop point must let the probe keep
// printing, and that is the only evidence no hit left a thread suspended — the debugger reports
// success either way.
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

public class CapturedProbe {

    static class Request {
        final String id;
        final int size;

        Request(String id, int size) {
            this.id = id;
            this.size = size;
        }

        // Counts its own invocations, so "nothing was invoked to render the snapshot" is a reading and
        // not a promise. Nothing else in this probe touches the counter.
        @Override public String toString() {
            toStringCalls++;
            return "Request(" + id + ")";
        }
    }

    // Strongly reachable for the JVM's whole life.
    static final Request PINNED = new Request("pinned-req", 42);

    // Cleared by the `drop` cue.
    static volatile Request doomed = new Request("doomed-req", 7);

    static volatile int toStringCalls = 0;
    static volatile int handled = 0;
    static volatile String last = "";
    static volatile boolean dropRequested = false;

    // Read from inside the anonymous class, which is what makes `javac` emit `this$0`: since JDK 18 the
    // back-reference is emitted only when the body actually needs the enclosing instance.
    //
    // The initializer is a concatenation and NOT a bare literal, and that is the whole point.
    // `final String owner = "literal"` is a *constant variable* (JLS 4.12.4) even as an instance field,
    // so `javac` folds every read of it into the constant pool — and with the read gone there is nothing
    // left needing the enclosing instance. JDK 17 emitted `this$0` anyway; JDK 21 does not, so the
    // literal version of this probe reproduced four captures on one JDK and three on the other while
    // looking identical. Caught by running the suite on two JDKs, which is what that rule is for.
    final String owner = "captured-probe-owner-" + Integer.toHexString(System.identityHashCode(this));

    Callable<String> task(final Request request, final String supplier, final int attempt) {
        return new Callable<String>() {
            @Override public String call() {
                handled++;
                // The traced line. Every capture is read here, because `javac` only emits a `val$`
                // field for a capture the body actually uses — a probe that named them and never read
                // them would compile to a class with nothing to find.
                String label = supplier + "/" + attempt + "/" + request.id + "/" + owner; // BP1
                last = label;
                return label;
            }
        };
    }

    // The control: an ordinary class, an ordinary method, no captures to find.
    static void plain(int n) {
        last = "plain-" + n; // BP2
    }

    static void startCueReader() {
        Thread cues = new Thread(() -> {
            try (java.io.BufferedReader in =
                         new java.io.BufferedReader(new java.io.InputStreamReader(System.in))) {
                String line;
                while ((line = in.readLine()) != null) {
                    // `contains` rather than `equals`, for the reason PoolProbe's cue reader records: a
                    // writer that prepends a BOM would otherwise be ignored silently.
                    if (line.contains("drop")) {
                        dropRequested = true;
                    }
                }
            } catch (Exception e) {
                // stdin closed; no cues left to read
            }
        }, "cue-reader");
        cues.setDaemon(true);
        cues.start();
    }

    // Its own method, and that is not tidiness. An interpreted frame's locals are ALL garbage-collection
    // roots, whether or not the code will read them again — so a `Request d = doomed;` left in main's
    // frame keeps the object alive across every System.gc() below, and the drop silently does nothing.
    // Nothing here holds a reference, so the only one left is the static field this clears.
    static void dropAndCollect() throws InterruptedException {
        doomed = null;
        // Two passes with a pause between them: one System.gc() is a request, and the second is what
        // makes a failure to collect a finding rather than a timing accident.
        System.gc();
        Thread.sleep(200);
        System.gc();
        Thread.sleep(200);
        System.out.println("dropped");
        System.out.flush();
    }

    public static void main(String[] args) throws Exception {
        CapturedProbe probe = new CapturedProbe();
        ExecutorService pool = Executors.newFixedThreadPool(2);
        startCueReader();

        // Readiness: both the class and the anonymous inner class have run, so a stop point armed after
        // this line resolves rather than deferring.
        pool.submit(probe.task(PINNED, "warmup", -1)).get();
        System.out.println("ready handled=" + handled);
        System.out.flush();

        boolean dropped = false;
        for (int i = 0; i < 100000; i++) {
            pool.submit(probe.task(PINNED, "supplier-A", i)).get();

            // Read straight out of the field into the call, with no local to hold it: see dropAndCollect.
            if (doomed != null) {
                pool.submit(probe.task(doomed, "supplier-B", i)).get();
            }

            if (dropRequested && !dropped) {
                dropped = true;
                dropAndCollect();
            }

            plain(i);
            System.out.println("tick " + i + " handled=" + handled + " last=" + last
                    + " toStringCalls=" + toStringCalls);
            System.out.flush();
            Thread.sleep(150);
        }
    }
}
