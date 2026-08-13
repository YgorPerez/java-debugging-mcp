// Probe for FILT-1 (thread filter on exception breakpoints / watchpoints), driven by
// mcp_integration.rs.
//
//   javac -g ThreadProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8811 -cp . ThreadProbe
//
// Two named threads each throw-and-swallow the SAME exception type in a loop, printing which thread
// threw. A thread-filtered exception breakpoint must report throws from only one of them — and the
// other thread must keep printing, proving it was never suspended by the filter.
public class ThreadProbe {

    // A dedicated type so an exception breakpoint can target it precisely (its subclasses aside).
    static class FilterException extends RuntimeException {
        FilterException(String m) { super(m); }
    }

    // Throw and swallow in one place, so a caught-exception breakpoint has a stable throw site.
    //
    // The stop-point marker below is on the throw itself, and it is a TRAILING comment on purpose: a
    // marker on its own line would shift every line number under it, and `probe_line` is what every test
    // here reads. Note also that this paragraph must not spell the marker, because `probe_line` returns the
    // FIRST line that contains it and would hand a test this comment's line number instead — measured, by
    // getting "No method contains line 19".
    //
    // `i` is a parameter, so it is in the local-variable table from the first instruction and a `condition`
    // can read it at the marked line without waiting for a statement to run (the same reason
    // `CondProbe.hot` takes one).
    static void doWork(String who, int i) {
        try {
            throw new FilterException(who + "-" + i); // BP1
        } catch (FilterException e) {
            // swallowed on purpose
        }
    }

    static Runnable worker(String who) {
        return () -> {
            int i = 0;
            while (true) {
                doWork(who, i);
                System.out.println(who + " throw " + i);
                i++;
                try {
                    Thread.sleep(150);
                } catch (InterruptedException e) {
                    return;
                }
            }
        };
    }

    public static void main(String[] args) throws Exception {
        Thread a = new Thread(worker("alpha"), "alpha-worker");
        Thread b = new Thread(worker("beta"), "beta-worker");
        a.setDaemon(true);
        b.setDaemon(true);
        a.start();
        b.start();
        Thread.sleep(600000);
    }
}
