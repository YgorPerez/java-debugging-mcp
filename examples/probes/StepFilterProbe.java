// Probe for STEP-1 (class filtering on step requests), driven by mcp_integration.rs.
//
//   javac -g StepFilterProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8832 -cp . StepFilterProbe
//
// The shape that makes a step filter testable is a call that goes THROUGH the JDK and comes back:
//
//   work()  ->  java.util.ArrayList.sort(...)  ->  compare()  [ours again]
//
// `sort` is real JDK code with real line numbers, so an unfiltered `step_into` at the marked line lands
// inside `java.util.*` — usually `ArrayList.sort` or `Arrays.sort` — and getting back out costs several
// more steps. With `java.*` excluded the same step lands on the next line of THIS class instead, which
// is the whole point of the feature and the only thing that distinguishes it from stepping being lucky.
//
// `compare` is deliberately our own method reached only from inside the JDK: it is what proves an
// exclusion suppresses the JDK frames without suppressing the callback into application code, which a
// blunter implementation (say, stepping OUT to the caller) would also swallow.
//
// The tick line is load-bearing as in the other probes: the loop keeps running, so a test can wait for
// the class to be loaded and running before it arms anything.
import java.util.ArrayList;
import java.util.List;

public class StepFilterProbe {

    static int comparisons = 0;

    // Ours, but every call to it arrives from inside java.util. An exclusion of `java.*` must not stop
    // execution reaching here.
    static int compare(Integer a, Integer b) {
        comparisons++;
        return a - b;
    }

    static int work(int i) {
        List<Integer> xs = new ArrayList<>();
        xs.add((i * 7) % 5);
        xs.add((i * 3) % 5);
        xs.add(i % 5);
        xs.sort(StepFilterProbe::compare); // BP1 — step_into here goes into java.util without a filter
        int total = xs.get(0) + xs.get(2); // BP2 — where a filtered step_into should land
        return total;
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            int t = work(i);
            System.out.println("tick " + i + " total=" + t + " comparisons=" + comparisons);
            Thread.sleep(150);
        }
    }
}
