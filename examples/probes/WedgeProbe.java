// Probe for DUMP-8 (#123): can a `trace_expr` on the BLOCKED half of a monitor stop leave a debuggee
// thread stuck inside an invocation the debugger has already given up on?
//
//   javac -g WedgeProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8808 -cp . WedgeProbe
//
// `MonitorProbe` cannot answer that question and this exists because of the two reasons why:
//
//   1. It has no method that ACQUIRES a contended lock and returns a value, so there is nothing a
//      `trace_expr` can name that would need the monitor being reported on.
//   2. Its longest hold is 400 ms, comfortably inside the 2000 ms invocation budget
//      (`DEFAULT_INVOKE_TIMEOUT_MS`). An invocation that merely waits 400 ms and then succeeds is not
//      the hazard — it is the hazard's harmless neighbour, and a probe that produced it would look
//      like evidence of safety.
//
// So: ONE lock, held for HOLD_MS — 3000 ms against a 2000 ms budget, and see HOLD_MS for why the margin
// is the whole hold rather than whatever the runner leaves of it — and a
// `synchronized` accessor on the lock object itself. A contender queues on the lock, which is where
// `MONITOR_CONTENDED_ENTER` fires; at that instant the thread does not own the monitor, and
// `lock.stamp()` cannot complete until the holder lets go.
//
// THE COUNTER IS THE EVIDENCE, and it is per-lock on purpose. `acquisitions` advances only when the
// contender gets all the way through its `synchronized` block, so a stall in the contender is visible
// from outside the debugger — which is the standard every other non-suspending assertion in this suite
// is held to. `stamp()` reads it rather than mutating anything, so observing the probe cannot change
// what the probe reports.
import java.util.concurrent.atomic.AtomicInteger;

public class WedgeProbe {

    /** Contended entries the contender has COMPLETED. The tick line carries it; a stall is visible there. */
    static final AtomicInteger acquisitions = new AtomicInteger();

    /**
     * The lock, and the trap on it.
     *
     * `stamp()` is `synchronized`, so calling it requires the monitor. On the `blocked` half of a
     * contended pair the hit thread is queued on that very monitor, so a `trace_expr` naming this is an
     * invocation that cannot proceed until the holder releases — and the debugger's budget expires long
     * before that. `name` sits beside it as the control: a plain field read needs no monitor and is the
     * expression a caller can always safely ask for.
     */
    static class Wedge {
        final String name = "wedge";

        synchronized int stamp() {
            return acquisitions.get();
        }
    }

    static final Wedge LOCK = new Wedge();

    /**
     * Held for 1000 ms longer than `DEFAULT_INVOKE_TIMEOUT_MS` (2000 ms), which is the whole point: an
     * invocation that needs this monitor must still be waiting when the debugger stops waiting for it.
     * A hold inside the budget would merely be slow, and would demonstrate the opposite of the claim.
     *
     * **The margin is the full hold, not a fraction of it**, because `hold()` does not start this clock
     * until the contender is genuinely BLOCKED — so the whole 3000 ms remains after the
     * `MONITOR_CONTENDED_ENTER` that triggers the capture. Without that ordering the remaining hold would
     * be a function of when the contender happened to arrive, and the margin would be whatever the runner
     * left of it; that is the shape TEST-38 found timing the runner instead of the lock. It is also what
     * lets this be 3000 rather than 4200 — the test costs a hold twice over, so the margin is paid for
     * in wall clock on every run of the suite.
     */
    static final long HOLD_MS = 3000;

    /**
     * Out of the lock between holds, so the contender's entry is a CONTENDED one — see MonitorProbe's
     * GAP_MS for why a holder that loops straight back into `monitorenter` starves its contender.
     */
    static final long GAP_MS = 100;

    /** Set inside the synchronized block, so true means the monitor is genuinely owned. */
    static volatile boolean held;

    static volatile Thread contender;

    /** Bound on the holder's wait, so a contender stuck inside a debugger invocation cannot wedge the HOLDER. */
    static final long BLOCK_WAIT_CAP_MS = 5000;

    static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /**
     * Own the lock for `HOLD_MS`, starting the clock only once the contender is genuinely BLOCKED.
     *
     * The cap matters more here than it does in `MonitorProbe`. A contender parked inside a debugger
     * invocation never reaches `BLOCKED` again, so an unbounded wait would stop the holder too and the
     * probe would go silent for reasons that have nothing to do with the hazard — a wedged probe and a
     * wedged thread would then look identical, which is exactly the confusion this file exists to avoid.
     */
    static void hold() {
        synchronized (LOCK) {
            held = true;
            long deadline = System.currentTimeMillis() + BLOCK_WAIT_CAP_MS;
            while (contender != null
                    && contender.getState() != Thread.State.BLOCKED
                    && System.currentTimeMillis() < deadline) {
                sleep(1);
            }
            sleep(HOLD_MS);
            held = false;
        }
        sleep(GAP_MS);
    }

    /** Queue on a demonstrably-owned lock, then acquire it. ENTER fires at the `synchronized`. */
    static void contend() {
        while (!held) {
            sleep(1);
        }
        synchronized (LOCK) {
            acquisitions.incrementAndGet();
        }
    }

    static Thread loopForever(Runnable body, String name) {
        Thread t = new Thread(() -> {
            while (true) {
                body.run();
            }
        }, name);
        t.setDaemon(true);
        t.start();
        return t;
    }

    public static void main(String[] args) throws Exception {
        contender = loopForever(WedgeProbe::contend, "wedge-contender");
        loopForever(WedgeProbe::hold, "wedge-holder");

        // Ask the JVM, don't guess: one completed contended entry means the shape this probe is for has
        // demonstrably happened, so a test arming against it is not waiting on events nothing produces.
        while (acquisitions.get() < 1) {
            sleep(10);
        }
        System.out.println("wedge ready acquisitions=" + acquisitions.get());

        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " acquisitions=" + acquisitions.get());
            Thread.sleep(150);
        }
    }
}
