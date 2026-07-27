// Probe for TEST-10 (#35): a pool that retires and replaces its workers CONTINUOUSLY, so a dump is
// always reading a thread list that has already gone stale.
//
//   javac -g ChurnProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8823 -cp . ChurnProbe
//
// `collect_dump_rows` reads each thread's status separately from the `AllThreads` that named it, and has
// an arm for the read failing — "a thread can die between `AllThreads` and the questions we ask about
// it". Nothing in the suite had ever produced one. Every other probe's threads are immortal for the
// length of the test, which makes the *normal* case on a request pool the one case never presented:
// `PoolProbe` does retire workers, but only on a `quiesce` cue and all at once, and between cues it is
// as fixed as the rest.
//
// **The churn rate is bounded on purpose, and that is the whole design.** A probe that spawned threads
// as fast as it could would make the live count a function of how loaded the host is, and a dump taken
// against it would find a different JVM every run — the flakiness the issue names explicitly. Instead:
//
//   STABLE = 8    workers that never retire, parked forever with a name of their own. A dump always has
//                 these to find, whatever the churn did, so an assertion has something that is still
//                 true when the reply arrives. Started LAST, so they are also the threads a dump that
//                 walks `AllThreads` creation order never reaches — see main().
//   BATCH  = 16   short-lived workers started every PERIOD_MS, each living LIFE_MS. With
//                 LIFE_MS = 3 x PERIOD_MS the live churn population settles at ~3 batches — about 48 —
//                 and stays there rather than growing, while ~160 threads a second retire underneath
//                 any dump that is running.
//
// So the debuggee's *shape* is steady (roughly 48 churn + 8 stable + main) while its *membership* turns
// over completely several times a second. That is the state a real pool is in whenever anyone dumps it,
// and it is the one where "the thread list is a snapshot of a moment that has passed" stops being a
// pedantic remark and becomes the thing that decides what the reply says.
//
// main prints the heartbeat rather than each worker: at this rate a line per thread would be hundreds a
// second, and the counters are what a test needs anyway — `created` says churn is happening at all, and
// its rate of change says it is still happening now.
import java.util.concurrent.atomic.AtomicLong;

public class ChurnProbe {

    /// Workers that never retire — the stable thing a dump can be asserted about.
    static final int STABLE = 8;

    /// Live churn workers at any moment. Each slot holds exactly one, so this IS the population.
    static final int SLOTS = 48;

    /// How long one churn worker lives before handing its slot to a fresh thread.
    static final long LIFE_MS = 300;

    static final AtomicLong created = new AtomicLong();
    static final AtomicLong retired = new AtomicLong();

    static final Object GATE = new Object();

    /// The bottom of a stable worker's stack: parked forever, holding the frames above it. `wait()`
    /// rather than a spin, so eight extra threads cost no CPU that the dump would otherwise be
    /// competing for.
    static void park() {
        synchronized (GATE) {
            try {
                GATE.wait();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    static void stableWork() {
        park();
    }

    /// A churn worker's whole life: two frames, a short sleep, then the thread exits and its id becomes
    /// something the debugger may still be holding but the JVM no longer has a thread for.
    static void serve(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        // Counted just before the run method returns rather than after the thread is truly gone, which
        // nothing inside the thread can observe. Close enough for "churn is happening", which is all a
        // test reads it for.
        retired.incrementAndGet();
    }

    /// Fill one slot: a fresh thread serves for `LIFE_MS`, starts its own replacement, and exits.
    ///
    /// A self-replacing chain rather than a submit loop, because it is the shape that makes the rate a
    /// property of the probe instead of of the host. The population is exactly `SLOTS` — the replacement
    /// starts before the incumbent returns — and the death rate is `SLOTS / LIFE_MS`, whatever else the
    /// machine is doing.
    static void fillSlot() {
        Thread t = new Thread(() -> {
            serve(LIFE_MS);
            fillSlot(); // hand the slot on, then let this thread die
        }, "churn-worker-" + created.incrementAndGet());
        t.setDaemon(true);
        t.start();
    }

    static void startCollector() {
        Thread t = new Thread(() -> {
            while (true) {
                System.gc();
                try {
                    Thread.sleep(100);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                }
            }
        }, "collector");
        t.setDaemon(true);
        t.start();
    }

    public static void main(String[] args) throws Exception {
        startCollector();

        // Staggered, and that is the difference between a probe that reproduces the state and one that
        // reproduces it a third of the time. Started together, all SLOTS threads would also DIE together
        // — one burst every LIFE_MS — and a dump lasting tens of milliseconds would miss the burst more
        // often than it hit it. Spread over one lifetime, a thread retires every LIFE_MS / SLOTS ms
        // instead, so any dump long enough to be worth taking overlaps several deaths.
        for (int i = 0; i < SLOTS; i++) {
            fillSlot();
            Thread.sleep(LIFE_MS / SLOTS);
        }

        // The stable workers are started LAST, on purpose (DUMP-3, #43). `AllThreads` hands the debugger
        // threads in creation order and a dump walks that order until `limit`, so the threads a caller
        // actually came to look at are the ones a default `limit: 40` runs out before reaching: here the
        // first ~55 ids are the JVM's own housekeeping and the churn population, and every
        // `stable-worker` sits behind them. That is what was measured on a real WildFly — 267 threads,
        // and the default dump reached none of the request pool — so a probe that put its interesting
        // threads first would be the one shape that cannot reproduce it. The churn slots keep creating
        // ids after these, but only ever *after*, so the property holds for the life of the probe.
        for (int i = 0; i < STABLE; i++) {
            Thread t = new Thread(ChurnProbe::stableWork, "stable-worker-" + i);
            t.setDaemon(true);
            t.start();
        }

        for (int i = 0; i < 100000000; i++) {
            System.out.println("tick " + i + " created=" + created.get()
                    + " retired=" + retired.get() + " live=" + Thread.activeCount());
            Thread.sleep(250);
        }
    }
}
