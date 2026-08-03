// Probe for DUMP-7 (#96) — lock contention that RESOLVES, so all four MONITOR_* events fire.
//
//   javac -g MonitorProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8826 -cp . MonitorProbe
//
// `ContendedProbe` is deliberately the opposite shape and cannot be reused here. Its holders take a lock
// and never give it back — which is exactly right for a *dump*, where the question is "what is wedged right
// now", but it means `MONITOR_CONTENDED_ENTER` fires once per waiter and `MONITOR_CONTENDED_ENTERED` NEVER
// fires, because no waiter ever gets in. The two events are the ends of one contended entry, and half a
// pair yields no duration at all. So this probe contends and then RELEASES, over and over.
//
// Four locks, each a distinct CLASS rather than a bare Object, for the reason `ContendedProbe`'s header
// gives: a snapshot naming `MonitorProbe$FastLock@…` can be checked against the thread that should be on
// it, where four `java.lang.Object@…` entries could be paired any way at all and still look right. Here it
// does a second job — a `ClassOnly` modifier on a monitor request takes a reference type, so a filter test
// needs four types to tell apart.
//
//   FastLock   held HOLD_FAST_MS (60)   ┐ two contended entries with durations an order of magnitude
//   SlowLock   held HOLD_SLOW_MS (400)  ┘ apart, so a min_duration_ms threshold between them can be shown
//                                         to KEEP one and DROP the other. A single duration cannot: a
//                                         threshold that filtered everything and one that filtered nothing
//                                         would both look like success.
//   TimeoutLock  wait(WAIT_TIMEOUT_MS)  → MONITOR_WAITED with timed_out = TRUE  (nobody ever notifies)
//   NotifyLock   wait(5000) + notifyAll → MONITOR_WAITED with timed_out = FALSE (notified in ~30ms)
//
// Both readings of `timed_out` on purpose. It is the one piece of outcome the wire carries, "nobody
// signalled it" and "it was signalled" are opposite diagnoses, and a probe that only ever timed out would
// pass against an implementation that hard-coded the flag.
//
// **The contention is a handshake, not a sleep.** Each holder sets a volatile flag INSIDE its synchronized
// block, so the flag can only be true while the monitor is genuinely owned; the contender spins on the flag
// and only then reaches its own `synchronized`, where it blocks for the remainder of the hold. A contender
// that raced ahead of its holder would acquire an unheld lock, produce no ENTER/ENTERED pair at all, and
// leave the test waiting on events that were never generated. The loop repeats forever, so an occasional
// lost race costs one iteration rather than the run.
//
// main ticks with the counts on the line — `tick 12 blocked=41 waited=88` — which is the only evidence a
// trace-mode assertion has that the VM was never suspended: none of the six worker threads could report
// that themselves, and a suspended VM stops the tick.
import java.util.concurrent.atomic.AtomicInteger;

public class MonitorProbe {

    /** Distinct lock classes, so a snapshot's label identifies WHICH lock — and so `ClassOnly` has types. */
    static class FastLock {}

    static class SlowLock {}

    static class TimeoutLock {}

    static class NotifyLock {}

    static final Object FAST = new FastLock();
    static final Object SLOW = new SlowLock();
    static final Object TIMEOUT = new TimeoutLock();
    static final Object NOTIFY = new NotifyLock();

    /** How long each holder keeps its lock. An order of magnitude apart — see the header. */
    static final long HOLD_FAST_MS = 60;
    static final long HOLD_SLOW_MS = 400;

    /**
     * How long a holder stays OUT of its lock after releasing it, and the single most important number
     * here — measured, not guessed. With no gap at all the first version of this probe produced ONE
     * contended entry in thirteen seconds: `synchronized` is unfair on HotSpot, so a holder that loops
     * straight back into `monitorenter` barges the thread already queued on it, over and over. The queued
     * contender is not starved by bad luck, it is starved systematically, and the probe still LOOKS right
     * from the outside — it prints ticks and its counters move.
     *
     * Small relative to either hold, so the shape stays "contended entry" rather than becoming an
     * uncontended one: the contender is already blocked when the holder releases, and the holder is not
     * competing for the lock during this window.
     */
    static final long GAP_MS = 20;

    /** Short enough that the waiter times out repeatedly rather than once a minute. */
    static final long WAIT_TIMEOUT_MS = 40;

    /** Long enough that the notify always arrives first, so `timed_out` is reliably false. */
    static final long NOTIFY_WAIT_MS = 5000;

    static final long NOTIFY_EVERY_MS = 30;

    /** Set INSIDE the synchronized block, so true means the monitor is really owned. One per lock. */
    static volatile boolean fastHeld;
    static volatile boolean slowHeld;

    /** Contended entries completed, and waits returned — printed on the tick line. */
    static final AtomicInteger blocked = new AtomicInteger();
    static final AtomicInteger waited = new AtomicInteger();

    static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** Own `lock` for `holdMs`, flagging ownership from inside the block so no contender can race ahead. */
    static void hold(Object lock, long holdMs, boolean fast) {
        synchronized (lock) {
            if (fast) {
                fastHeld = true;
            } else {
                slowHeld = true;
            }
            sleep(holdMs);
            if (fast) {
                fastHeld = false;
            } else {
                slowHeld = false;
            }
        }
        // OUTSIDE the block, and load-bearing — see GAP_MS.
        sleep(GAP_MS);
    }

    /** Queue on a lock that is demonstrably owned, then acquire it once the holder lets go. */
    static void contend(Object lock, boolean fast) {
        while (!(fast ? fastHeld : slowHeld)) {
            sleep(1);
        }
        synchronized (lock) { // ENTER fires here; ENTERED fires when the holder releases
            blocked.incrementAndGet();
        }
    }

    /** `wait()` that always expires: nothing ever notifies TIMEOUT. */
    static void waitOut() {
        synchronized (TIMEOUT) {
            try {
                TIMEOUT.wait(WAIT_TIMEOUT_MS); // WAIT fires here; WAITED reports timed_out = true
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        waited.incrementAndGet();
    }

    /** `wait()` that is notified well before its timeout, so `timed_out` reads false. */
    static void waitNotified() {
        synchronized (NOTIFY) {
            try {
                NOTIFY.wait(NOTIFY_WAIT_MS); // WAIT fires here; WAITED reports timed_out = false
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        waited.incrementAndGet();
    }

    static void notifier() {
        while (true) {
            sleep(NOTIFY_EVERY_MS);
            synchronized (NOTIFY) {
                NOTIFY.notifyAll();
            }
        }
    }

    static void loopForever(Runnable body, String name) {
        Thread t = new Thread(() -> {
            while (true) {
                body.run();
            }
        }, name);
        t.setDaemon(true);
        t.start();
    }

    public static void main(String[] args) throws Exception {
        loopForever(() -> hold(FAST, HOLD_FAST_MS, true), "fast-holder");
        loopForever(() -> contend(FAST, true), "fast-contender");
        loopForever(() -> hold(SLOW, HOLD_SLOW_MS, false), "slow-holder");
        loopForever(() -> contend(SLOW, false), "slow-contender");
        loopForever(MonitorProbe::waitOut, "timeout-waiter");
        loopForever(MonitorProbe::waitNotified, "notify-waiter");
        loopForever(() -> notifier(), "notifier");

        // Ask the JVM, don't guess: readiness here means contention and waiting have DEMONSTRABLY happened,
        // not that the threads were started. A test that armed against a probe which had not yet contended
        // would be waiting on events nothing had produced, and would blame the arming.
        while (blocked.get() < 1 || waited.get() < 1) {
            sleep(10);
        }
        System.out.println("monitors ready blocked=" + blocked.get() + " waited=" + waited.get());

        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " blocked=" + blocked.get() + " waited=" + waited.get());
            Thread.sleep(150);
        }
    }
}
