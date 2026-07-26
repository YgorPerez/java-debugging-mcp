// Probe for #17 (the thread dump's suspension budget), driven by mcp_integration.rs.
//
//   javac -g ManyThreadsProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8820 -cp . ManyThreadsProbe
//
// Enough live threads, deep enough, that a dump cannot read them all inside a tiny suspension budget.
// That is the whole point: the budget's early exit has to be provable, and a test that merely hoped a
// dump was slow would be flaky. With a budget of 1ms the dump stops after the first thread or two and
// must report the rest as unexamined.
//
// The threads park in a `synchronized` wait rather than spinning, so they hold a real stack without
// burning CPU while suspended — a busy-spinning pool would make the dump's own timings noisy.
//
// `main` prints the tick line and is deliberately NOT one of the many: it is the thread that proves a
// budget-truncated dump still resumed the VM, and the workers cannot report that themselves because a
// dump that stopped early may never have read them at all.
public class ManyThreadsProbe {

    // Comfortably more than the 40-thread default limit, so the budget rather than the limit is what
    // stops a dump against it.
    static final int WORKERS = 60;

    static final Object GATE = new Object();

    // Three frames of depth per worker, so each one costs the dump real per-frame lookups.
    static void level3() {
        synchronized (GATE) {
            try {
                GATE.wait(); // parked here for the life of the probe
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    static void level2() {
        level3();
    }

    static void level1() {
        level2();
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < WORKERS; i++) {
            Thread t = new Thread(ManyThreadsProbe::level1, "worker-" + i);
            t.setDaemon(true);
            t.start();
        }
        // Let every worker reach its parked frame before anything dumps them.
        Thread.sleep(500);

        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " workers=" + WORKERS); // BP1
            Thread.sleep(150);
        }
    }
}
