// Probe for EXC-3 (#68) — one exception instance rethrown through several layers.
//
//   javac -g RethrowProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8815 -cp . RethrowProbe
//
// Reproduces, in the small, what an exception stop armed on a WildFly application type actually sees: an
// instance thrown once in application code and then rethrown at every layer of the EJB interceptor chain
// (`InterceptorContext.proceed`, `CMTTxInterceptor`, `PooledInstanceInterceptor`, `SecurityContext…`).
// One request produced 30 snapshots of a single instance that way, spending a `trace_max_hits: 30` budget
// entirely on plumbing and disarming the stop point mid-request.
//
// Shaped to hold the two properties that make the fix testable, and that a loop of fresh throws does not:
//
//   - **One instance, several throws.** Every layer rethrows `e` itself, so instance identity is what
//     separates a chain from four unrelated failures. ExcProbe constructs a new exception each iteration
//     and is therefore the control case.
//   - **A different site per rethrow.** Separate named methods rather than recursion, because "collapse
//     the middle" must be shown to keep BOTH ends — a wrapper that drops the cause at a *different* site
//     is the failure the swallowed-exception playbook exists for, so a blanket dedupe would be wrong.
//
// `origin` is the informative record: the only frame that is application code and the only one that knows
// why. `security` is where the exception escapes into `main`. Everything between is the plumbing.
public class RethrowProbe {

    static class LayerException extends RuntimeException {
        LayerException(String message) {
            super(message);
        }
    }

    static String status = "none";

    // The original throw. In the real case this is the one record worth reading, and it was the 9th.
    static void origin(int i) {
        throw new LayerException("origin failed on " + i); // BP_ORIGIN
    }

    static void pooled(int i) {
        try {
            origin(i);
        } catch (LayerException e) {
            throw e; // BP_POOLED — the same instance, a second site
        }
    }

    static void tx(int i) {
        try {
            pooled(i);
        } catch (LayerException e) {
            throw e; // BP_TX — the middle of the chain, and the part worth collapsing
        }
    }

    // The last rethrow before the exception leaves for main: the escape point.
    static void security(int i) {
        try {
            tx(i);
        } catch (LayerException e) {
            throw e; // BP_SECURITY
        }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            try {
                security(i);
            } catch (LayerException e) {
                status = "handled:" + i;
            }
            System.out.println("tick " + i + " " + status);
            Thread.sleep(150);
        }
    }
}
