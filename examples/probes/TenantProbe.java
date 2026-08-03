// Probe for TRACE-11 (several trace expressions in one snapshot), driven by mcp_integration.rs.
//
//   javac -g TenantProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8820 -cp . TenantProbe
//
// This probe exists because the point of TRACE-11 is not "record a value" but "record a DISAGREEMENT",
// and a disagreement needs both values captured at the same instant. It reproduces the shape the issue was
// filed about: the schema actually in use is carried in a static ThreadLocal, an unset one silently
// resolves to a default, and the session object carries the schema it BELIEVES it is using. There is no
// correlation id anywhere in the target codebase — MDC, requestId, correlationId and traceId are all zero
// hits — so two independently budgeted snapshot streams cannot be joined after the fact, which is exactly
// why one snapshot has to carry both.
//
// MISMATCH_PERIOD makes the disagreement periodic rather than constant, and that is load-bearing: a probe
// where the two values ALWAYS differed would pass even if the second expression were secretly reading the
// first. Two hits in every three disagree, one agrees, so a test can assert both outcomes appear.
//
// The tick line is load-bearing too, as in CallerProbe: a traced stop point must let the probe keep
// printing, which is what actually proves no thread was left suspended. A suspending one stops the ticks
// dead, and the debugger reports success either way.
public class TenantProbe {

    /** One hit in every this-many has the tenant's own schema set; the rest fall through to the default. */
    static final int MISMATCH_PERIOD = 3;

    /** The schema the thread is really serving. Unset is the bug: it resolves to `infotravel` in silence. */
    static final ThreadLocal<String> SCHEMA_IN_USE = new ThreadLocal<>();

    static final String DEFAULT_SCHEMA = "infotravel";
    static final String TENANT_SCHEMA = "orinter";

    /** What the session believes it is talking to — reached through a getter, so a trace_expr invokes. */
    static class Session {
        private final String nmSchema;

        Session(String nmSchema) {
            this.nmSchema = nmSchema;
        }

        String getNmSchema() {
            return nmSchema;
        }
    }

    /** The silent default. Reading this alone tells you nothing, which is the whole problem. */
    static String currentSchema() {
        String s = SCHEMA_IN_USE.get();
        return s == null ? DEFAULT_SCHEMA : s;
    }

    /**
     * One request. The traced line is the print, chosen because BOTH values are in scope there: `schema`
     * as a plain local (no invocation, so it works in a read-only session) and `sessao` behind a getter.
     */
    static void handle(Session sessao, int i) {
        String schema = currentSchema();
        System.out.println("handled " + schema + " tick " + i); // TRACE_LINE
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            if (i % MISMATCH_PERIOD == 0) {
                SCHEMA_IN_USE.set(TENANT_SCHEMA);
            } else {
                SCHEMA_IN_USE.remove();
            }
            handle(new Session(TENANT_SCHEMA), i);
            Thread.sleep(150);
        }
    }
}
