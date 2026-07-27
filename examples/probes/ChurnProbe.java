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
// A retiring worker leaves the JVM in one of **two** states a debugger can meet, and which one it is
// decides what the reply says — so the probe decides it, rather than the garbage collector (TEST-19, #54).
// See HELD.
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
    /// Sets the DEATH RATE — `SLOTS / LIFE_MS`, ~80 a second — and nothing else. It used to set more than
    /// that, and TEST-19 (#54) is what it cost: see HELD.
    ///
    /// It is deliberately not tuned against the clock any more. What it still has to be is *short enough
    /// that several workers die inside any dump worth taking* — at 600ms across 48 staggered slots one
    /// retires every 12.5ms, so even a dump that finishes in 100ms overlaps eight deaths. Raising it far
    /// enough that a dump overlaps none would make both halves of TEST-10 unreachable, and that is the
    /// only bound on it.
    static final long LIFE_MS = 600;

    /// How many retired workers HELD keeps at once. At ~40 held retirements a second it laps in about six
    /// seconds — two orders of magnitude longer than a dump of this probe, and bounded so that a probe
    /// left running for a whole suite does not accumulate dead `Thread`s without limit.
    static final int HELD_SLOTS = 512;

    /// Retired workers whose `Thread` object this probe deliberately keeps reachable — a ring, so the
    /// retained set is bounded, and newest-wins.
    ///
    /// **This is the probe deciding which of two states a retiring worker leaves behind, instead of
    /// leaving it to the collector's timing (TEST-19, #54).** A JDWP thread id is a weak reference. When a
    /// worker exits, its `Thread` becomes unreachable and the next `System.gc()` invalidates the id, so a
    /// dump that listed the thread and then asks about it gets an error and drops the row. But if
    /// *something still holds the object*, the id stays valid and JDWP answers `ZOMBIE` — the thread is
    /// reported, and reported as finished. Both are states a real dump meets and both are asserted, so the
    /// probe has to present both.
    ///
    /// It used to present them by accident. Which one a dump saw depended on where each worker's death
    /// fell between the dump's `AllThreads` and the collector's next 100ms pass, which made the ZOMBIE
    /// half a function of **how long a dump takes** — and a dump takes longer on a loaded host, a slower
    /// JDK, an instrumented build, or a busier suite. Measured on JDK 11: a dump of this probe costs
    /// ~500ms idle and finds 5-6 zombies; under competing load the same dump costs ~950ms, by which point
    /// every worker the list named is not merely dead but collected, and it finds **none**. That is #54,
    /// and it is #43's recorded "worker lifetime tracks dump length" coupling one turn further on.
    ///
    /// Holding every second retirement breaks the coupling rather than re-tuning it. An odd-numbered
    /// worker is left collectible and still produces the dropped row; an even-numbered one is held and
    /// answers `ZOMBIE` for as long as it takes the ring to lap — ~6 seconds at this churn rate, against
    /// dumps measured in hundreds of milliseconds. The two populations alternate in creation order, which
    /// is the order a dump reads a name family in, so neither can end up clustered outside the window.
    ///
    /// **The failure mode is now the right way round.** A slower dump overlaps MORE deaths, so it finds
    /// more of both — where before it found more of one and fewer of the other, and a slow enough dump
    /// found no zombie at all. The load that used to break this test is now the condition it is easiest
    /// to observe under.
    static final Thread[] HELD = new Thread[HELD_SLOTS];

    static final AtomicLong created = new AtomicLong();
    static final AtomicLong retired = new AtomicLong();
    static final AtomicLong held = new AtomicLong();

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

    /// Keep a retiring worker's `Thread` reachable, so its JDWP id survives its thread. See HELD.
    ///
    /// The array element is written without synchronization on purpose: nothing ever *reads* HELD, and
    /// reachability is not a happens-before question — the collector sees the store whenever it runs, and
    /// that is the entire job. `held` is atomic only so that two workers retiring at once cannot pick the
    /// same slot and quietly drop one of them.
    static void hold(Thread t) {
        HELD[(int) (held.getAndIncrement() % HELD_SLOTS)] = t;
    }

    /// Fill one slot: a fresh thread serves for `LIFE_MS`, starts its own replacement, and exits.
    ///
    /// A self-replacing chain rather than a submit loop, because it is the shape that makes the rate a
    /// property of the probe instead of of the host. The population is exactly `SLOTS` — the replacement
    /// starts before the incumbent returns — and the death rate is `SLOTS / LIFE_MS`, whatever else the
    /// machine is doing.
    static void fillSlot() {
        final long id = created.incrementAndGet();
        Thread t = new Thread(() -> {
            serve(LIFE_MS);
            // Held BEFORE the slot is handed on, so a worker is reachable-after-death from the moment it
            // stops running rather than from some point after its successor has started.
            //
            // Every second one, and they all keep the same name — one `churn-worker-#` family, not two.
            // A second family would change how many slots the round-robin selection gives the pool
            // (DUMP-3, #43), which is a neighbouring test's subject, so the split is by number and not by
            // name.
            if ((id & 1L) == 0L) {
                hold(Thread.currentThread());
            }
            fillSlot(); // hand the slot on, then let this thread die
        }, "churn-worker-" + id);
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

        // `held=` is the one a test reads as a PRECONDITION rather than as colour (TEST-19, #54). A
        // debugger cannot ask "is there a finished thread I would still be able to resolve?" — the answer
        // is a property of the debuggee's reference graph, and asking for it is the race. So the debuggee
        // says. A non-zero `held` is the probe stating that it is holding that many retired workers'
        // `Thread` objects, which is the state the `[zombie]` half of TEST-10 needs to exist; a test that
        // reads it has established its precondition instead of racing for it.
        for (int i = 0; i < 100000000; i++) {
            System.out.println("tick " + i + " created=" + created.get()
                    + " retired=" + retired.get() + " live=" + Thread.activeCount()
                    + " held=" + Math.min(held.get(), HELD_SLOTS));
            Thread.sleep(250);
        }
    }
}
