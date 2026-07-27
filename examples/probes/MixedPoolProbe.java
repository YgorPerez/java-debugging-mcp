// Probe for TEST-8 (#24): a pool whose threads are in DIFFERENT code, which is the case the line-table
// cache's headline number does not cover.
//
//   javac -g MixedPoolProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8822 -cp . MixedPoolProbe
//
// `PoolShapeProbe` puts all 300 workers in the SAME 60 frames, which is the cache's best case: 240
// line tables serve every thread. The honest objection is that a real app server is not that uniform — its
// workers are spread across handlers — and if the win depended on uniformity it would not survive contact
// with the 8180.
//
// The bracket is known at both ends already. All threads in identical code costs 1,625 packets (measured);
// no two frames sharing a (class, method) pair costs 21,364, because that is the pre-cache measurement —
// with nothing shared, the cache never hits. This probe measures where a REALISTIC pool falls between them:
//
//   FRAMEWORK    = 40  frames every request shares — a filter chain, security, dispatch. Cached once for
//                       the whole dump however many threads are in it.
//   ENDPOINTS    = 10  distinct handlers, one class each, so the workers are genuinely in different code.
//   PER_ENDPOINT = 20  frames below the shared prefix, unique to one handler.
//
// So 300 threads cover 40 + 10x20 = 240 distinct (class, method) pairs rather than
// 60. If the shared prefix is what carries the cache — which is the claim — the cost should land
// close to the uniform case, and nowhere near the unshared one.
//
// The framework frames are the OUTER ones and the handler frames the inner, as in a real request: the stack
// reads main -> framework -> handler -> parked.
public class MixedPoolProbe {

    static final int WORKERS = 300;
    static final int ENDPOINTS = 10;

    static final Object GATE = new Object();

    /// Park forever, holding every frame above. The bottom of every worker's stack, whichever handler it is in.
    static void park() {
        synchronized (GATE) {
            try {
                GATE.wait();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    /// Launch the pool and tick. `main`'s own thread is never one of the workers, so the tick line stays
    /// the evidence that the VM was resumed after a dump.
    public static void main(String[] args) throws Exception {
        for (int i = 0; i < WORKERS; i++) {
            final int endpoint = i % ENDPOINTS;
            Thread t = new Thread(() -> Framework.f39(endpoint), "http-nio-8180-exec-" + i);
            t.setDaemon(true);
            t.start();
        }
        // Every worker must be parked at its depth before anything dumps them.
        Thread.sleep(2000);
        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " workers=" + WORKERS + " endpoints=" + ENDPOINTS); // BP1
            Thread.sleep(150);
        }
    }
}

/// The frames every request shares, whatever it is handling. One class, 40 methods — cached once per dump.
class Framework {
    /// Bottom of the shared prefix: hand off to whichever handler this worker serves.
    static void f0(int endpoint) { Dispatch.into(endpoint); }
    static void f1(int e) { f0(e); }
    static void f2(int e) { f1(e); }
    static void f3(int e) { f2(e); }
    static void f4(int e) { f3(e); }
    static void f5(int e) { f4(e); }
    static void f6(int e) { f5(e); }
    static void f7(int e) { f6(e); }
    static void f8(int e) { f7(e); }
    static void f9(int e) { f8(e); }
    static void f10(int e) { f9(e); }
    static void f11(int e) { f10(e); }
    static void f12(int e) { f11(e); }
    static void f13(int e) { f12(e); }
    static void f14(int e) { f13(e); }
    static void f15(int e) { f14(e); }
    static void f16(int e) { f15(e); }
    static void f17(int e) { f16(e); }
    static void f18(int e) { f17(e); }
    static void f19(int e) { f18(e); }
    static void f20(int e) { f19(e); }
    static void f21(int e) { f20(e); }
    static void f22(int e) { f21(e); }
    static void f23(int e) { f22(e); }
    static void f24(int e) { f23(e); }
    static void f25(int e) { f24(e); }
    static void f26(int e) { f25(e); }
    static void f27(int e) { f26(e); }
    static void f28(int e) { f27(e); }
    static void f29(int e) { f28(e); }
    static void f30(int e) { f29(e); }
    static void f31(int e) { f30(e); }
    static void f32(int e) { f31(e); }
    static void f33(int e) { f32(e); }
    static void f34(int e) { f33(e); }
    static void f35(int e) { f34(e); }
    static void f36(int e) { f35(e); }
    static void f37(int e) { f36(e); }
    static void f38(int e) { f37(e); }
    static void f39(int e) { f38(e); }
}

/// Routes to one of the handler classes, so each worker descends into different code.
class Dispatch {
    static void into(int endpoint) {
        switch (endpoint) {
            case 0: Handler0.h19(); return;
            case 1: Handler1.h19(); return;
            case 2: Handler2.h19(); return;
            case 3: Handler3.h19(); return;
            case 4: Handler4.h19(); return;
            case 5: Handler5.h19(); return;
            case 6: Handler6.h19(); return;
            case 7: Handler7.h19(); return;
            case 8: Handler8.h19(); return;
            case 9: Handler9.h19(); return;
            default: MixedPoolProbe.park();
        }
    }
}

/// Handler 0: 20 frames no other handler shares.
class Handler0 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 1: 20 frames no other handler shares.
class Handler1 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 2: 20 frames no other handler shares.
class Handler2 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 3: 20 frames no other handler shares.
class Handler3 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 4: 20 frames no other handler shares.
class Handler4 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 5: 20 frames no other handler shares.
class Handler5 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 6: 20 frames no other handler shares.
class Handler6 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 7: 20 frames no other handler shares.
class Handler7 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 8: 20 frames no other handler shares.
class Handler8 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

/// Handler 9: 20 frames no other handler shares.
class Handler9 {
    static void h0() { MixedPoolProbe.park(); }
    static void h1() { h0(); }
    static void h2() { h1(); }
    static void h3() { h2(); }
    static void h4() { h3(); }
    static void h5() { h4(); }
    static void h6() { h5(); }
    static void h7() { h6(); }
    static void h8() { h7(); }
    static void h9() { h8(); }
    static void h10() { h9(); }
    static void h11() { h10(); }
    static void h12() { h11(); }
    static void h13() { h12(); }
    static void h14() { h13(); }
    static void h15() { h14(); }
    static void h16() { h15(); }
    static void h17() { h16(); }
    static void h18() { h17(); }
    static void h19() { h18(); }
}

