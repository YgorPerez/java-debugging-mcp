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
//   HOUSEKEEPING  a pool of parked threads started FIRST and never retired, standing in for the
//        = 40     furniture an app server puts up before it will take a request — Undertow's I/O
//                 selectors, the service container, the deployment scanner. Deliberately larger than
//                 `debug.thread_dump`'s default `limit`, because on the WildFly this reproduces that is
//                 what ate all forty slots (DUMP-3, #43). See main().
//   STABLE = 8    workers that never retire, parked forever with a name of their own. A dump always has
//                 these to find, whatever the churn did, so an assertion has something that is still
//                 true when the reply arrives. Started LAST, so they are also the threads a dump that
//                 walks `AllThreads` creation order never reaches — see main().
//   SLOTS  = 48   short-lived workers, one per slot, each living LIFE_MS and starting its replacement
//                 as it goes. The live churn population therefore IS `SLOTS` rather than something the
//                 host's load decides, while ~80 threads a second retire underneath any dump that is
//                 running.
//
// So the debuggee's *shape* is steady (~103 threads: 40 housekeeping + 48 churn + 8 stable + main and
// the JVM's own) while its *membership* turns over completely a couple of times a second. That is the
// state a real pool is in whenever anyone dumps it, and it is the one where "the thread list is a
// snapshot of a moment that has passed" stops being a pedantic remark and becomes the thing that decides
// what the reply says.
//
// main prints the heartbeat rather than each worker: at this rate a line per thread would be hundreds a
// second, and the counters are what a test needs anyway — `created` says churn is happening at all, and
// its rate of change says it is still happening now.
import java.util.concurrent.atomic.AtomicLong;

public class ChurnProbe {

    /// Workers that never retire — the stable thing a dump can be asserted about.
    static final int STABLE = 8;

    /// Long-lived threads created before anything else, so that the interesting ones are out of reach of
    /// a creation-order dump (DUMP-3, #43). Must stay above `debug.thread_dump`'s default `limit` of 40.
    static final int HOUSEKEEPING = 40;

    /// Live churn workers at any moment. Each slot holds exactly one, so this IS the population.
    static final int SLOTS = 48;

    /// How long one churn worker lives before handing its slot to a fresh thread.
    ///
    /// **Tied to how long a dump of this probe takes, not chosen for its own sake.** The state TEST-10
    /// asserts — a thread that finished during the dump and is still readable as `[zombie]` — only exists
    /// between a worker's death and the next `System.gc()`. A dump reads the churn population LAST (they
    /// are always the newest threads, so a compacted live list always puts them at the end), so a worker
    /// is read roughly `threads x per-thread-cost` after the list was taken; if that is longer than its
    /// whole life plus a GC interval, every one of them is not merely dead but already collected, and the
    /// `[zombie]` half of the test stops being reachable at all.
    ///
    /// It went that way once: adding the 40 HOUSEKEEPING threads for DUMP-3 (#43) lengthened a dump of
    /// this probe by ~60%, and at LIFE_MS = 300 twelve dumps in a row found no zombie. 600 puts the deaths
    /// back inside the window with room either side — the earliest workers are read long enough after
    /// dying to have been collected (the dropped-row half), the later ones recently enough to still
    /// answer.
    static final long LIFE_MS = 600;

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

        // The furniture, up before anything else and never taken down (DUMP-3, #43). Forty of them,
        // against a default `limit` of forty: a dump that reads `AllThreads` in order and stops at the
        // limit spends every slot in here and never reaches a single thread this probe was started to
        // present. That is the WildFly reading exactly — 267 threads, 40 slots, all of them JVM
        // internals, MSC service threads and Undertow selectors, while 13 request workers sat 328 frames
        // deep and unread.
        //
        // **They have to be immortal, and the churn population is why we know that.** The stable eight
        // were originally placed last on their own, on the assumption that the ~55 ids ahead of them
        // would hold the position. They do not: HotSpot's live thread list is COMPACTED as threads die,
        // so once the first churn generation had retired the stable workers had moved to the front of it
        // — measured at ids 0x8..0xf, inside the default limit, with the current churn generation behind
        // them. A short-lived predecessor buys nothing. Only a thread that is still alive when the dump
        // runs can stand between the caller and what they came for, which is precisely why an app server
        // reproduces this and a burst of work does not.
        for (int i = 0; i < HOUSEKEEPING; i++) {
            Thread t = new Thread(ChurnProbe::stableWork, "io-selector-" + i);
            t.setDaemon(true);
            t.start();
        }

        // Staggered, and that is the difference between a probe that reproduces the state and one that
        // reproduces it a third of the time. Started together, all SLOTS threads would also DIE together
        // — one burst every LIFE_MS — and a dump lasting tens of milliseconds would miss the burst more
        // often than it hit it. Spread over one lifetime, a thread retires every LIFE_MS / SLOTS ms
        // instead, so any dump long enough to be worth taking overlaps several deaths.
        for (int i = 0; i < SLOTS; i++) {
            fillSlot();
            Thread.sleep(LIFE_MS / SLOTS);
        }

        // The stable workers are started LAST, on purpose (DUMP-3, #43). They are the request pool: the
        // threads a caller actually came to look at, sitting behind ~47 immortal ones that a
        // creation-order dump spends its whole limit on. A probe that put its interesting threads first
        // would be the one shape that cannot reproduce the finding.
        //
        // The churn slots keep creating ids after these, but only ever *after*, so the stable eight are
        // never the newest either — which is what rules out "just walk the list backwards" as the fix.
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
