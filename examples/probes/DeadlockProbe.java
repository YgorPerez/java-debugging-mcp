// Probe for DUMP-1 (all-thread stacks + monitor ownership), driven by mcp_integration.rs.
//
//   javac -g DeadlockProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8818 -cp . DeadlockProbe
//
// A deliberate two-lock deadlock, which is the only shape that proves the monitor reporting actually
// correlates: two threads each holding one lock and waiting for the other.
//
//   deadlock-one  holds LockA, blocked entering LockB
//   deadlock-two  holds LockB, blocked entering LockA
//
// The locks are distinct CLASSES rather than two bare Objects so the dump names them — `LockA` vs
// `LockB` in the output is checkable, whereas two `java.lang.Object@<id>` entries could be paired
// backwards and still look right.
//
// The `armed` barrier is load-bearing: without it one thread can take both locks and finish, and the
// deadlock never forms. Each thread takes its FIRST lock, announces itself, and only reaches for the
// second once both have announced — so the cycle is guaranteed, not raced for. It is an AtomicInteger
// rather than a volatile int because two `++`s on a volatile can both write 1, leaving the barrier
// stuck at 1 forever and the threads spinning instead of deadlocking.
//
// main() keeps printing a tick line while the other two are wedged, which is what proves a dump that
// suspended the VM really resumed it: the deadlocked threads can't report that themselves.
import java.util.concurrent.atomic.AtomicInteger;

public class DeadlockProbe {

    static class LockA {}

    static class LockB {}

    static final Object LOCK_A = new LockA();
    static final Object LOCK_B = new LockB();
    static final AtomicInteger armed = new AtomicInteger();

    static void grab(Object first, Object second) {
        synchronized (first) {
            armed.incrementAndGet();
            // Wait until the other thread also holds its first lock, so the cycle is certain.
            while (armed.get() < 2) {
                Thread.onSpinWait();
            }
            synchronized (second) { // never entered: this is the wedge
                throw new IllegalStateException("unreachable — the two threads must deadlock");
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Thread one = new Thread(() -> grab(LOCK_A, LOCK_B), "deadlock-one");
        Thread two = new Thread(() -> grab(LOCK_B, LOCK_A), "deadlock-two");
        // Daemons, so a wedged pair can never keep the JVM alive after the harness kills main.
        one.setDaemon(true);
        two.setDaemon(true);
        one.start();
        two.start();

        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " armed=" + armed.get()); // BP1
            Thread.sleep(150);
        }
    }
}
