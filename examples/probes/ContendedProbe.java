// Probe for TEST-10 (#35): contention at the scale a real server has it — dozens of threads queued on a
// handful of locks, rather than the clean two-thread cycle `DeadlockProbe` presents.
//
//   javac -g ContendedProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8824 -cp . ContendedProbe
//
// `DeadlockProbe` is the right shape for proving that the monitor reporting *correlates at all*: two
// threads, two locks, and a cross-pairing that a report which merely listed monitors per thread would
// get wrong. What it cannot show is whether the correlation still picks the RIGHT holder when there is
// more than one candidate. With two threads and two locks there is only ever one other thread to name,
// so `← held by 0x…` is right by construction — every wrong answer is also the right one.
//
// Here there are four locks, four holders and forty-eight waiters, and every waiter is blocked on a lock
// that three other threads in the same dump are also holding one of. Naming the holder now means finding
// the one row out of four whose `holds` list contains this waiter's contended monitor; getting it wrong
// is possible, and therefore worth asserting.
//
//   LOCKS            = 4    distinct lock CLASSES (Lock0…Lock3), not four bare Objects, so the dump names
//                           them: "ContendedProbe$Lock2@…" can be checked against the waiter that should
//                           be on it, where four "java.lang.Object@…" entries could be paired any way at
//                           all and still look right. Same reasoning as `DeadlockProbe`'s LockA/LockB.
//   WAITERS_PER_LOCK = 12   comfortably "more than one waiter", and enough that a report which named the
//                           holder for the first waiter and lost it for the rest would show up.
//
// **The holders park WITHOUT releasing**, which is the one thing that makes this shape hard to write by
// accident: `Object.wait()` — what every other parking probe here uses — releases the monitor, so a
// holder that parked that way would hold nothing and there would be no contention to find.
// `CountDownLatch.await()` inside the `synchronized` block blocks the thread while it keeps the lock,
// which is what a real thread doing slow work under a lock looks like.
//
// The barrier is exact rather than timed. main holds every waiter's `Thread` and polls `getState()`
// until all forty-eight report BLOCKED, then prints `contended=<n>/<n> armed`. Announcing from inside
// each waiter would have to happen *before* it blocks — there is no code left to run afterwards — so it
// would say "about to contend" and leave the test racing the JVM into the monitor. Asking the JVM
// instead removes the race entirely.
//
// main keeps ticking while the other 52 threads are wedged, which is the only evidence that a dump which
// suspended the VM really resumed it: none of the wedged threads can report that themselves.
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

public class ContendedProbe {

    /// Distinct lock classes, so the dump's label identifies WHICH lock rather than just "an Object".
    static class Lock0 {}

    static class Lock1 {}

    static class Lock2 {}

    static class Lock3 {}

    static final Object[] LOCKS = {new Lock0(), new Lock1(), new Lock2(), new Lock3()};

    static final int WAITERS_PER_LOCK = 12;

    /// Never counted down. The holders block on it forever, inside their `synchronized` blocks.
    static final CountDownLatch FOREVER = new CountDownLatch(1);

    /// Released once every holder owns its lock, so no waiter can win the race to an unheld monitor and
    /// quietly become the holder itself.
    static final CountDownLatch HOLDERS_READY = new CountDownLatch(LOCKS.length);

    static final AtomicInteger blocked = new AtomicInteger();

    /// Take the lock and never give it back. Two frames, so the dump has a stack to show as well as a
    /// lock line.
    static void hold(Object lock) {
        synchronized (lock) {
            HOLDERS_READY.countDown();
            try {
                // NOT Object.wait(): that would release the monitor, and this probe would present no
                // contention at all while looking exactly the same from the outside.
                FOREVER.await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    static void holderEntry(Object lock) {
        hold(lock);
    }

    /// Queue on a lock somebody else is holding, and stay queued.
    static void waitFor(Object lock) {
        synchronized (lock) { // never entered: this is where the waiter piles up
            throw new IllegalStateException("unreachable — the holder never releases");
        }
    }

    static void waiterEntry(Object lock) {
        waitFor(lock);
    }

    public static void main(String[] args) throws Exception {
        for (int k = 0; k < LOCKS.length; k++) {
            final Object lock = LOCKS[k];
            Thread t = new Thread(() -> holderEntry(lock), "holder-" + k);
            t.setDaemon(true);
            t.start();
        }
        // Every lock must be OWNED before anyone queues on it.
        HOLDERS_READY.await();

        Thread[] waiters = new Thread[LOCKS.length * WAITERS_PER_LOCK];
        for (int k = 0; k < LOCKS.length; k++) {
            final Object lock = LOCKS[k];
            for (int i = 0; i < WAITERS_PER_LOCK; i++) {
                Thread t = new Thread(() -> waiterEntry(lock), "waiter-" + k + "-" + i);
                t.setDaemon(true);
                waiters[k * WAITERS_PER_LOCK + i] = t;
                t.start();
            }
        }

        // Ask the JVM, don't guess: a waiter cannot announce that it is blocked, because blocking is the
        // last thing it does.
        while (blocked.get() < waiters.length) {
            int n = 0;
            for (Thread t : waiters) {
                if (t.getState() == Thread.State.BLOCKED) {
                    n++;
                }
            }
            blocked.set(n);
            if (n < waiters.length) {
                Thread.sleep(20);
            }
        }
        System.out.println("contended=" + blocked.get() + "/" + waiters.length + " armed");

        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " contended=" + blocked.get()); // BP1
            Thread.sleep(150);
        }
    }
}
