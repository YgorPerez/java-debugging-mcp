// Probe for EVAL-12 (#112) — resolving a ONE-SEGMENT name against the frame's own class.
//
//   javac -g BareNameProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8815 -cp . BareNameProbe
//
// In Java source, a bare `calls` inside a method of the class that declares `static int calls` **is**
// `BareNameProbe.calls` — no qualification. This probe exercises the four ways a one-segment name can
// resolve, and the order between them, because the order is the whole of the design:
//
//   local → field of `this` → static of the frame's declaring class → failure
//
// Each shape gets its own method so a traced stop point can carry one `trace_expr` naming it:
//
//   localWins()        a local `shadowed` shadowing everything else       -> 30
//   instanceWins()     no local; `this.shadowed` hides the inherited one  -> 20
//   inheritedStatic()  a static declared on the SUPERCLASS                -> 7
//   tick()             a STATIC method reading a static of its own class  -> the issue's own case
//   unknownSite()      a name that is nowhere                              -> the improved message
//
// `shadowed` is deliberately declared twice — `static` on `Base`, instance on `Child`. Java permits
// that (the field hides rather than overrides) and it is the only way to put a static and an instance
// field of the same name in scope at one point, which is what makes the middle two rows a real
// ordering test rather than two independent lookups.
//
// `tick()` is the shape the issue was actually filed about, and it is the one with no `this` at all:
// a static method, so the resolver cannot fall back on an instance field and the static step is the
// only thing that can answer.
public class BareNameProbe {

    static class Base {
        static int shadowed = 10;
        static int inherited = 7;
    }

    static class Child extends Base {
        int shadowed = 20;

        void localWins() {
            int shadowed = 30;
            System.out.println("localWins " + shadowed); // BP1
        }

        void instanceWins() {
            System.out.println("instanceWins " + shadowed); // BP2
        }

        void inheritedStatic() {
            System.out.println("inheritedStatic " + inherited); // BP3
        }
    }

    static int calls = 0;

    static void tick() {
        calls++;
        System.out.println("tick " + calls); // BP4
    }

    // Its own site purely so the "name resolves to nothing" case does not have to share a line with
    // BP4. Two traced stop points on ONE line is #102 (BP-6), where only one of them records — an open
    // bug, and entangling a resolution test with it would make this test fail for the wrong reason.
    static void unknownSite() {
        System.out.println("unknownSite"); // BP5
    }

    public static void main(String[] a) throws Exception {
        Child c = new Child();
        System.out.println("ready");
        for (int i = 0; i < 100000; i++) {
            c.localWins();
            c.instanceWins();
            c.inheritedStatic();
            tick();
            unknownSite();
            Thread.sleep(150);
        }
    }
}
