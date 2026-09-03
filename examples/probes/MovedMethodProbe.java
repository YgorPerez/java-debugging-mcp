// Probe for BP-10 (#202) — a redeploy in which the target METHOD MOVED, which `RedeployProbe` cannot do.
//
//   javac -g MovedMethodProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8834 -cp . MovedMethodProbe
//
// `RedeployProbe` (BP-7, #115) loads the same name twice and defines BOTH copies from the SAME bytes, so
// the method is at the same line in each and a stop point that reused the first copy's line would still
// land correctly. That is the gap #202 named: the test over it passes whether or not a later copy's
// location is re-resolved, because there is nothing for the resolution to get wrong.
//
// **The two copies differ by a LINE TABLE, and by nothing else that matters.** `WidgetA` and `WidgetB` have
// identical bodies and are declared at different depths in this file, so `javac` gives them identical
// bytecode and different `LineNumberTable`s. The "v2" deployment loads `WidgetB`'s bytes with the UTF-8
// constant `WidgetB` rewritten to `WidgetA` — equal length, so every offset in the class file stays exactly
// where it was, which is the same surgery `TwinMemberProbe` uses on a field and a method name. One
// substitution is enough because every reference to the name goes through those UTF-8 entries: `this_class`,
// the `InnerClasses` simple name, and the constructor's own descriptor in the local variable table.
//
// So both deployments hold a class called `MovedMethodProbe$WidgetA`, one with `work` near the top of the
// file and one with `work` a couple of dozen lines lower — a redeploy in which the method moved, which is
// the ordinary result of editing anything above it.
//
// **The line each copy is really at is printed by the PROBE, not assumed by the test.** `work` reads its own
// line out of a `Throwable`'s stack trace and returns it, so a run where the surgery silently produced two
// identical line tables says so instead of passing for the wrong reason. It is the same discipline
// `TwinMemberProbe` applies with its `lacks` checks.
//
// The second load is on a CUE rather than a timer, for `RedeployProbe`'s reason: the stop point has to be
// armed while only v1 exists, and a timer racing the arm would make a green run mean nothing.
import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.lang.reflect.Method;

public class MovedMethodProbe {

    // Reached only by reflection, exactly as in `RedeployProbe`: naming the type anywhere in this file
    // would let the app classloader define a THIRD copy, and the stop point would arm on that too.
    public static class WidgetA {

        private final String owner;

        public WidgetA(String owner) {
            this.owner = owner;
        }

        public String work(int i) {
            int at = new Throwable().getStackTrace()[0].getLineNumber(); // WORK_A
            return owner + ":" + i + "@" + at;
        }
    }

    // Byte-for-byte `WidgetA` apart from its name and its line numbers. Kept textually identical on
    // purpose — an edit to one that is not made to the other changes the bytecode as well as the line
    // table, and then a difference in behaviour has two candidate causes instead of one.
    public static class WidgetB {

        private final String owner;

        public WidgetB(String owner) {
            this.owner = owner;
        }

        public String work(int i) {
            int at = new Throwable().getStackTrace()[0].getLineNumber(); // WORK_B
            return owner + ":" + i + "@" + at;
        }
    }

    static final String WIDGET = "MovedMethodProbe$WidgetA";

    // One instance per "deployment". Parent-less, so it cannot delegate up to the app loader and collapse
    // the two deployments into one type — which is the case that does NOT reproduce the bug.
    static final class DeployLoader extends ClassLoader {
        private final String tag;
        private final boolean moved;

        DeployLoader(String tag, boolean moved) {
            super(null);
            this.tag = tag;
            this.moved = moved;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(WIDGET)) {
                throw new ClassNotFoundException(name);
            }
            String from = moved ? "MovedMethodProbe$WidgetB" : WIDGET;
            try (InputStream in = MovedMethodProbe.class.getResourceAsStream("/" + from + ".class")) {
                if (in == null) {
                    throw new ClassNotFoundException("no " + from + ".class on the classpath");
                }
                ByteArrayOutputStream buf = new ByteArrayOutputStream();
                byte[] chunk = new byte[4096];
                for (int n; (n = in.read(chunk)) > 0; ) {
                    buf.write(chunk, 0, n);
                }
                byte[] bytes = buf.toByteArray();
                if (moved) {
                    rename(bytes, "WidgetB", "WidgetA");
                }
                return defineClass(name, bytes, 0, bytes.length);
            } catch (ClassNotFoundException e) {
                throw e;
            } catch (Exception e) {
                throw new ClassNotFoundException(name, e);
            }
        }

        // Equal-length in-place substitution, as in `TwinMemberProbe`. Every occurrence is rewritten, which
        // is what keeps `this_class`, the `InnerClasses` simple name and the constructor's descriptor all
        // agreeing after the rename; unequal lengths would move every offset after the entry and the class
        // would not verify, so the two names are chosen to match.
        private static void rename(byte[] bytes, String from, String to) {
            byte[] f = from.getBytes(java.nio.charset.StandardCharsets.UTF_8);
            byte[] t = to.getBytes(java.nio.charset.StandardCharsets.UTF_8);
            if (f.length != t.length) {
                throw new IllegalArgumentException(from + " and " + to + " must be the same length");
            }
            int found = 0;
            for (int i = 0; i + f.length <= bytes.length; i++) {
                boolean hit = true;
                for (int j = 0; j < f.length; j++) {
                    if (bytes[i + j] != f[j]) {
                        hit = false;
                        break;
                    }
                }
                if (hit) {
                    System.arraycopy(t, 0, bytes, i, t.length);
                    found++;
                }
            }
            if (found == 0) {
                throw new IllegalStateException("nothing to rename for " + from);
            }
        }

        @Override
        public String toString() {
            return "MovedLoader(" + tag + ")";
        }
    }

    public static void main(String[] args) throws Exception {
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));

        Class<?> v1 = new DeployLoader("v1", false).loadClass(WIDGET);
        Object i1 = v1.getConstructor(String.class).newInstance("v1");
        Method w1 = v1.getMethod("work", int.class);
        System.out.println("deployed v1");

        // The retired deployment is kept reachable on purpose (`i1` and `w1`). An undeployed module whose
        // loader is genuinely unreachable would eventually be collected and take its copy with it; the case
        // that costs time is the one where it is not.
        Object i2 = null;
        Method w2 = null;

        for (int i = 0; i < 100000; i++) {
            System.out.println("ran " + w1.invoke(i1, i));
            if (w2 != null) {
                System.out.println("ran " + w2.invoke(i2, i));
            }
            // Polled rather than blocking, so the loop keeps the v1 copy hot while the test decides.
            if (w2 == null && in.ready()) {
                String line = in.readLine();
                if (line != null && "redeploy".equals(line.trim())) {
                    Class<?> v2 = new DeployLoader("v2", true).loadClass(WIDGET);
                    System.out.println("second copy is a different type=" + (v1 != v2));
                    System.out.println("second copy is the same name=" + v2.getName().equals(v1.getName()));
                    i2 = v2.getConstructor(String.class).newInstance("v2");
                    w2 = v2.getMethod("work", int.class);
                    // The premise, stated by the JVM: the method really is at a different line in the two
                    // copies. Without this a run where the surgery produced identical line tables would
                    // pass every assertion below while proving nothing.
                    int at1 = lineOf(w1.invoke(i1, 0));
                    int at2 = lineOf(w2.invoke(i2, 0));
                    System.out.println("work moved=" + (at1 != at2) + " from=" + at1 + " to=" + at2);
                    System.out.println("deployed v2");
                }
            }
            System.out.println("tick " + i);
            Thread.sleep(150);
        }
    }

    /** The `@<line>` tail `work` appends to its answer. */
    static int lineOf(Object answer) {
        String s = String.valueOf(answer);
        return Integer.parseInt(s.substring(s.lastIndexOf('@') + 1));
    }
}
