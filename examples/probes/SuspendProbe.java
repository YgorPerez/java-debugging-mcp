// Probe for SAFE-11 (per-thread suspend), driven by mcp_integration.rs.
//
//   javac -g SuspendProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8812 -cp . SuspendProbe
//
// THREE named worker threads, each printing its OWN tick counter:
//
//   worker-a tick 17 layout-adturismo
//   worker-b tick 12 layout-adturismo
//   worker-c tick 12 layout-adturismo
//
// That per-thread counter is the whole point of this probe, and it is the only honest witness for
// what SAFE-11 claims. Suspending one thread must stop THAT thread's ticks while the other two keep
// counting; a shared counter, or a single "tick" line, could not tell "we froze one worker" apart
// from "we froze the JVM" — and every tool reports success either way, which is exactly how the bugs
// in this repo's safety work survived five review rounds.
//
// `layoutLoginMap` and `lookup` exist for the payoff case from the issue: reading a Map subscript is
// an INVOKE, and an invoke needs a suspended thread. It is modelled on the real question this tool was
// built for — LayoutSrv.layoutLoginMap["ADTURISMO"] on the shared 8180 — so the test can prove the
// capability against a thread frozen on its own rather than against a whole-VM freeze.
//
// `ephemeral-worker` exists so a test has a FINISHED thread to point at, deterministically rather than
// by winning a race against a churning pool. It announces itself, lives long enough to be listed, then
// ends — and `main` keeps a strong reference to it, so the JVM cannot collect the Thread object and
// JDWP goes on answering ZOMBIE for its id. That is the difference between **finished** and
// **vanished** made reproducible: this probe can produce the first, and only a stale id produces the
// second.
import java.util.LinkedHashMap;
import java.util.Map;

public class SuspendProbe {

    // A populated Map, so a subscript has something to find and something to miss.
    static final Map<String, String> layoutLoginMap = new LinkedHashMap<String, String>();
    static {
        layoutLoginMap.put("ADTURISMO", "layout-adturismo");
        layoutLoginMap.put("ORINTER", "layout-orinter");
    }

    // A plain getter, so `debug.evaluate` has an invoke that is not a subscript either.
    static String lookup(String key) {
        String v = layoutLoginMap.get(key);
        return v == null ? "MISSING" : v;
    }

    static final class Ephemeral extends Thread {
        Ephemeral() { super("ephemeral-worker"); }
        @Override public void run() {
            System.out.println("ephemeral-worker ready");
            try {
                Thread.sleep(6000);
            } catch (InterruptedException e) {
                // fall through and end
            }
            System.out.println("ephemeral-worker done");
        }
    }

    // The loop body lives in a NAMED method of SuspendProbe rather than in the anonymous Runnable, so
    // `class_pattern: "SuspendProbe"` can arm a line breakpoint on it. An anonymous class is a separate
    // reference type (SuspendProbe$1) and the resolver answers "no method contains line N" for the outer
    // one, which reads like a bad line number rather than a wrong class.
    static void runWorker(String who) {
        int n = 0;
        while (true) {
            // Touch the map on every pass, so the class is loaded and warm before any test asks about
            // it — a class loads on first use, and evaluating against one that has not been used yet
            // answers a different question (TEST-17).
            String layout = lookup("ADTURISMO");
            n++;
            System.out.println(who + " tick " + n + " " + layout);   // BP1
            try {
                Thread.sleep(120);
            } catch (InterruptedException e) {
                return;
            }
        }
    }

    static Runnable worker(final String who) {
        return new Runnable() {
            @Override public void run() {
                runWorker(who);
            }
        };
    }

    public static void main(String[] args) throws Exception {
        // Held in a local for the whole run: a collected Thread object would make this id VANISHED,
        // and the point of it is to be FINISHED.
        Ephemeral gone = new Ephemeral();
        gone.start();

        for (String who : new String[] { "worker-a", "worker-b", "worker-c" }) {
            Thread t = new Thread(worker(who), who);
            t.setDaemon(true);
            t.start();
        }
        Thread.sleep(600000);
        System.out.println("main done " + gone.getName());
    }
}
