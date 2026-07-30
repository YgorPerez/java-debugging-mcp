// Probe for BP-4 (#78) — a source line inside a `finally` block, driven by mcp_integration.rs.
//
//   javac -g FinallyProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8830 -cp . FinallyProbe
//
// `javac` does not compile a `finally` body once and jump to it. It **inlines a copy per exit path**,
// so the single source line below appears in the line table more than once — measured on Temurin 11 and
// 17 as `line N: 24` (normal completion) and `line N: 39` (the exception path). A breakpoint resolver
// that takes the first line-table match therefore arms the normal-completion copy only, and the stop
// point fires on the calls that SUCCEEDED and stays silent on the one that failed.
//
// That is the worst possible failure direction for this tool: a `finally` is the idiomatic logpoint
// site precisely because the request and the response are both still in scope on both paths, and in
// `it-pagamento` 22 of the 23 outbound payment-gateway choke points sit inside one. The silence is
// indistinguishable from "the code never ran".
//
//   call(i, false)  returns normally  -> the finally runs with rs = "OK"
//   call(i, true)   throws            -> the finally runs with rs = null
//
// `rs` is what separates the two copies, so a trace on the marked line records "OK" from one and null
// from the other. Both are printed by the probe itself as well: the debugger reports success either
// way, so the probe's own stdout is the only evidence that both paths actually executed.
public class FinallyProbe {

    // Distinct from any JDK type so nothing else in the JVM can be mistaken for this probe's throw.
    static class GatewayException extends RuntimeException {
        GatewayException(String message) {
            super(message);
        }
    }

    static String call(int i, boolean fail) {
        String rq = "REQ-" + i;
        String rs = null;
        try {
            if (fail) {
                throw new GatewayException("gateway refused " + i);
            }
            rs = "OK";
            return rs;
        } finally {
            System.out.println("finally rq=" + rq + " rs=" + rs); // BP1
        }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            // The normal path first, so a resolver that arms only the first line-table copy still looks
            // like it is working — the test has to distinguish "fired" from "fired on both paths".
            call(i, false);
            try {
                call(i, true);
            } catch (GatewayException e) {
                // Swallowed, exactly like the catch blocks this tool exists to see into.
            }
            System.out.println("tick " + i); // BP2
            Thread.sleep(150);
        }
    }
}
