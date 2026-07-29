// Probe for EXC-2 (#67) — the message the JVM already computed, read off the exception.
//
//   javac -g ExcMsgProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8815 -cp . ExcMsgProbe
//
// ExcProbe already covers an exception constructed WITH a message, so this one reproduces the two cases
// it cannot:
//
//   helpfulNpe(i)  a real null dereference, so the message belongs to the JVM rather than to this file.
//                  On JDK 15+ (JEP 358, on by default) it names the failing subexpression outright —
//                  `because the return value of "ExcMsgProbe$Detail.getCount()" is null` — which is the
//                  sentence #67 exists to surface. On JDK 11 the same throw carries NO message, and that
//                  difference is the point rather than a nuisance: both branches are reachable from CI.
//   messageless(i) an exception carrying no message on ANY JDK, so "no message" can be distinguished
//                  from "an empty message" and from "we failed to read it".
//
// Both throws are CAUGHT, for the same reason ExcProbe's is: the tick line is what proves a traced
// exception stop left nothing suspended, and it can only keep printing if the probe survives its throws.
public class ExcMsgProbe {

    // The null the NPE is about is a *return value*, not a local — that is the shape a helpful NPE
    // describes most usefully, and the shape the investigation behind #67 actually hit.
    static class Detail {
        Integer count = null;

        Integer getCount() {
            return count;
        }
    }

    // A custom type so the messageless case pins itself to this class's ref type and can't be confused
    // with an internal JVM throw. `super()` with no argument is the whole content of the fixture.
    static class Bare extends RuntimeException {
        Bare() {
            super();
        }
    }

    static String npeStatus = "none";
    static String bareStatus = "none";

    static void helpfulNpe(int i) {
        try {
            Detail d = new Detail();
            int n = d.getCount(); // BP_NPE — unboxing a null Integer, so the JVM writes the message
            npeStatus = "unreachable:" + n;
        } catch (NullPointerException e) {
            npeStatus = "npe:" + i;
        }
    }

    static void messageless(int i) {
        try {
            throw new Bare(); // BP_BARE
        } catch (Bare e) {
            bareStatus = "bare:" + i;
        }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            helpfulNpe(i);
            messageless(i);
            System.out.println("tick " + i + " " + npeStatus + " " + bareStatus);
            Thread.sleep(150);
        }
    }
}
