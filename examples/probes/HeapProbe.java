// Probe for DISC-10 (the JDWP heap-query family), driven by mcp_integration.rs.
//
//   javac -g HeapProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8831 -cp . HeapProbe
//
// The heap is SHAPED so each question has a distinguishable answer rather than a plausible one:
//
//   Widget      : 7 strongly reachable
//   SubWidget   : 2 strongly reachable, extending Widget
//                 -> Instances(Widget) answers 7 if exact-type, 9 if subtype-inclusive. That is the
//                    single semantic most likely to mislead a caller, so the probe makes the two
//                    readings numerically different instead of leaving them indistinguishable.
//   unreachable : 3 Widgets allocated and dropped, so "only strongly-reachable objects are reported"
//                 is measured (7, not 10) rather than assumed.
//   Ballast     : enough live objects that a full walk has real work to do — the whole point, since
//                 the cost of these commands tracks the LIVE HEAP and not the answer.
//
// `Nothing` exists and is never instantiated: a loaded class with zero live instances is a different
// answer from a class that does not resolve, and both have to be reachable from one run.
//
// THE TICK LINE IS THE MEASUREMENT. A tick is the only evidence an application thread is running — the
// debugger reports success either way — so the gap between two ticks is the pause the debugger imposed.
// The probe prints its own measured gap in milliseconds, which is what makes "this stops the world" a
// reading off the debuggee rather than a claim by the thing that caused it.
import java.util.ArrayList;
import java.util.List;

public class HeapProbe {

    static class Widget {
        final int id;
        String payload;

        Widget(int id) {
            this.id = id;
            this.payload = "widget-" + id;
        }

        @Override public String toString() {
            return "Widget#" + id;
        }
    }

    static class SubWidget extends Widget {
        SubWidget(int id) {
            super(id);
        }

        @Override public String toString() {
            return "SubWidget#" + id;
        }
    }

    // Loaded, never instantiated. Its count must be 0 and must read as an answer.
    static class Nothing {
        int unused;
    }

    static class Ballast {
        int a;
        long b;

        Ballast(int a) {
            this.a = a;
            this.b = a;
        }
    }

    static final List<Widget> WIDGETS = new ArrayList<>();
    static final List<SubWidget> SUBWIDGETS = new ArrayList<>();
    static final List<Ballast> BALLAST = new ArrayList<>();

    // Big enough that a walk is measurably more than the tick interval, small enough that a test does
    // not spend its budget allocating. `docs/heap-query-measurements.md` measured 2,000,000 at 522ms and
    // 20,000 at 54ms against a 50ms baseline; this sits between them deliberately, because the finding
    // under test is "the pause tracks the heap", not any one number.
    static final int BALLAST_COUNT = Integer.getInteger("probe.ballast", 600_000);

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 7; i++) {
            WIDGETS.add(new Widget(i));
        }
        for (int i = 100; i < 102; i++) {
            SUBWIDGETS.add(new SubWidget(i));
        }
        // Allocated and dropped on purpose. If these ever appear, the command is reading the heap
        // without a collection and the "strongly reachable only" rule is wrong.
        for (int i = 900; i < 903; i++) {
            Widget doomed = new Widget(i);
            doomed.payload = null;
        }
        // Force the class to load without giving it an instance.
        Class.forName("HeapProbe$Nothing");

        for (int i = 0; i < BALLAST_COUNT; i++) {
            BALLAST.add(new Ballast(i));
        }

        System.out.println("ready widgets=" + WIDGETS.size()
                + " subwidgets=" + SUBWIDGETS.size()
                + " ballast=" + BALLAST.size());
        System.out.flush();

        // nanoTime, not the wall clock, because the wall clock can step and this number is evidence.
        long prev = System.nanoTime();
        while (true) {
            Thread.sleep(50);
            long now = System.nanoTime();
            long gapMs = (now - prev) / 1_000_000L;
            prev = now;
            System.out.println("tick " + (now / 1_000_000L) + " gap=" + gapMs + "ms");
            System.out.flush();
        }
    }
}
