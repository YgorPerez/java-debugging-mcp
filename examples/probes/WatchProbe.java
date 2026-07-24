// Probe for WATCH-1 (field watchpoints), driven by examples/test_watchpoint.rs.
//
//   javac -g WatchProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8797 -cp . WatchProbe
//
// Two fields are mutated in a loop from two named methods, so a watchpoint hit can be checked
// against a known mutating location and a known old→new pair:
//   - static int  counter  — bumped by bumpCounter(), so old+1 == new every time
//   - instance String label on the static `holder` — set by relabel() alternating two values
// `readOnly` is never written after class init, only read, so an access watch has something to fire
// on that a modification watch must ignore. It is deliberately NOT `static final`: javac inlines a
// constant primitive at the use site, and an inlined read issues no getstatic, so a FIELD_ACCESS
// watch on a `static final int` would never fire at all.
public class WatchProbe {

    static int counter = 0;
    static int readOnly = 41;
    static Holder holder = new Holder();

    static class Holder {
        String label = "start";
        int touched = 0;
    }

    // The only writer of `counter`. A modification watch must report THIS method.
    static void bumpCounter() {
        counter = counter + 1;
    }

    // The only writer of `holder.label`, alternating so old != new on every hit.
    static void relabel(int i) {
        holder.label = (i % 2 == 0) ? "even" : "odd";
    }

    // Reads readOnly without ever writing it — fodder for an access watch.
    static int readConfig() {
        return readOnly + holder.touched;
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            bumpCounter();
            relabel(i);
            int seen = readConfig();
            System.out.println("tick " + counter + " " + holder.label + " " + seen);
            Thread.sleep(150);
        }
    }
}
