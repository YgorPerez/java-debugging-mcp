// Probe for TEST-8 (#24): a thread pool shaped like a real application server's, rather than sized for
// a test's convenience.
//
//   javac -g PoolShapeProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8821 -cp . PoolShapeProbe
//
// #24's premise is that every shared-instance default was calibrated on loopback against
// `ManyThreadsProbe` — 60 threads, 3 frames deep — and that the real 8180 is "hundreds of threads, not
// 60" with stacks "far deeper than 8". Two of those three variables are properties of the DEBUGGEE, not
// of the network, so they can be reproduced here exactly: thread count is a loop bound and stack depth is
// a call chain. The third, latency, is supplied by `LatencyRelay` in the test harness. Between them there
// is no part of a "production-shaped instance" this suite cannot present.
//
// The numbers sit at the LOW end of what an app server presents, so a finding cannot be dismissed as an
// extreme:
//
//   WORKERS = 300   A WildFly/Tomcat default max-threads is 200-512. `ManyThreadsProbe`'s 60 is a
//                   fraction of the smallest of those.
//   DEPTH   = 60    A servlet request stack through a filter chain, security, JPA and a connection pool
//                   runs 60-150 frames deep. `max_frames`' default of 8 sees the top of it.
//
// **The depth is 60 DISTINCT methods, not one recursive call, and that is load-bearing.** A dump's
// per-frame cost is one `Method.LineTable` round trip, and the debugger caches line tables per dump keyed
// by (class, method) — so a recursive chain would collapse to a single lookup and make the cache look
// perfect. A real request stack has ~as many distinct methods as frames. With 60 of them the cache's win
// comes from reuse ACROSS the 300 threads, which is exactly where a request pool's reuse really is.
// (The chain runs once per thread and never returns, so it stays interpreted — the JVM has no reason to
// inline it, and the frames are genuinely on the stack.)
//
// Workers park in a `synchronized` wait rather than spinning, as `ManyThreadsProbe` does: a parked thread
// holds a real stack without burning CPU, so the dump's own timings are not competing with the debuggee
// for four cores. The point is to measure the DEBUGGER's per-frame cost.
//
// `main` is deliberately not one of the workers: it prints the tick line, which is the only thing that can
// show the VM was resumed after a dump that may have stopped early and never read the workers at all.
public class PoolShapeProbe {

    /// Low end of an app server's pool, and 5x `ManyThreadsProbe`.
    static final int WORKERS = 300;

    /// Frames below the thread's entry point before it parks. Low end of a servlet request stack.
    static final int DEPTH = 60;

    static final Object GATE = new Object();

    /// The bottom of every worker's stack: park forever, holding the frames above.
    static void f0() {
        synchronized (GATE) {
            try {
                GATE.wait(); // parked here for the life of the probe
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    static void f1() { f0(); }
    static void f2() { f1(); }
    static void f3() { f2(); }
    static void f4() { f3(); }
    static void f5() { f4(); }
    static void f6() { f5(); }
    static void f7() { f6(); }
    static void f8() { f7(); }
    static void f9() { f8(); }
    static void f10() { f9(); }
    static void f11() { f10(); }
    static void f12() { f11(); }
    static void f13() { f12(); }
    static void f14() { f13(); }
    static void f15() { f14(); }
    static void f16() { f15(); }
    static void f17() { f16(); }
    static void f18() { f17(); }
    static void f19() { f18(); }
    static void f20() { f19(); }
    static void f21() { f20(); }
    static void f22() { f21(); }
    static void f23() { f22(); }
    static void f24() { f23(); }
    static void f25() { f24(); }
    static void f26() { f25(); }
    static void f27() { f26(); }
    static void f28() { f27(); }
    static void f29() { f28(); }
    static void f30() { f29(); }
    static void f31() { f30(); }
    static void f32() { f31(); }
    static void f33() { f32(); }
    static void f34() { f33(); }
    static void f35() { f34(); }
    static void f36() { f35(); }
    static void f37() { f36(); }
    static void f38() { f37(); }
    static void f39() { f38(); }
    static void f40() { f39(); }
    static void f41() { f40(); }
    static void f42() { f41(); }
    static void f43() { f42(); }
    static void f44() { f43(); }
    static void f45() { f44(); }
    static void f46() { f45(); }
    static void f47() { f46(); }
    static void f48() { f47(); }
    static void f49() { f48(); }
    static void f50() { f49(); }
    static void f51() { f50(); }
    static void f52() { f51(); }
    static void f53() { f52(); }
    static void f54() { f53(); }
    static void f55() { f54(); }
    static void f56() { f55(); }
    static void f57() { f56(); }
    static void f58() { f57(); }
    static void f59() { f58(); }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < WORKERS; i++) {
            // Named like a real pool's workers, so a name_filter test has something realistic to match.
            Thread t = new Thread(PoolShapeProbe::f59, "http-nio-8180-exec-" + i);
            t.setDaemon(true);
            t.start();
        }
        // Let every worker reach its parked frame before anything dumps them: a thread still descending
        // would be measured mid-chain, which is neither shape.
        Thread.sleep(2000);

        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " workers=" + WORKERS + " depth=" + DEPTH); // BP1
            Thread.sleep(150);
        }
    }
}
