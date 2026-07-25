// Probe for METH-1 (method-exit reporting with return values), driven by mcp_integration.rs.
//
//   javac -g ReturnProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8819 -cp . ReturnProbe
//
// `classify` has TWO return statements and takes both on alternate iterations, which is the shape the
// whole feature exists for: with several returns, "what did it actually return?" is otherwise a
// guessing game — you have to pick a `return` to break on and hope it is the one that ran.
//
//   classify(even) -> returns "OK"    (the first return)
//   classify(odd)  -> returns null    (the second — the IntegraSrv.post-style non-200 path)
//
// A null return is deliberately one of the two: it is the value a swallowed-failure path actually
// produces, and it must be reported as `null` rather than as nothing at all.
//
// `other()` exists only so the class has a SECOND method that also returns. JDWP's ClassMatch fires on
// every method of a matching class, so a `method` filter that did nothing would pick this up too — it is
// how the test proves the filter is real rather than incidental.
//
// The tick line is load-bearing, as in the other trace probes: a traced method-exit request must let the
// probe keep printing, which is what proves no thread was left suspended.
public class ReturnProbe {

    static int calls = 0;

    // Two returns, one of them null. The `n` argument identifies which path a given hit came from.
    static String classify(int n) {
        calls++;
        if (n % 2 == 0) {
            return "OK";
        }
        return null;
    }

    // A second returning method, so an unfiltered request is visibly noisier than a filtered one.
    static int other() {
        return 7;
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            String even = classify(i * 2);
            String odd = classify(i * 2 + 1);
            int o = other();
            System.out.println("tick " + i + " calls=" + calls + " even=" + even + " odd=" + odd + " o=" + o);
            Thread.sleep(150);
        }
    }
}
