// Probe for EVAL-13 (#116) — one class name, two classloaders, and a member on only ONE of them.
//
//   javac -g TwinMemberProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8832 -cp . TwinMemberProbe
//
// `TwinLoaderProbe` already loads a name twice, but it defines both copies from the SAME bytes, so every
// member exists on both and no lookup can miss. That is the wrong shape for this bug: what happens on a
// WildFly redeploy is that the retired deployment's module classloader keeps its copy loaded while the new
// one defines a copy with a member the old one never had — a method whose signature just changed from two
// parameters to four, say. The evaluator resolved the name to whichever copy sorted first, failed to find
// the member there, and reported "has no static method … accepting 4 argument(s) of these types": true of
// the copy it inspected, false of the copy serving requests, and naming neither.
//
// **The two copies differ by byte surgery on the constant pool, not by a second source file.** The harness
// compiles one source file per probe, and generating genuinely different bytecode for one name inside one
// file is otherwise impossible. Three equal-length substitutions rewrite the UTF8 entries — a field name, a
// method name and a string literal — which keeps every offset in the class file exactly where it was:
//
//     markerAAA -> markerBBB     the static field
//     senseAAA  -> senseBBB      the static method
//     AAA-value -> BBB-value     the field's value, so the two copies are distinguishable in a reply
//
// So the `alpha` copy has `markerAAA` / `senseAAA()` and NOT `markerBBB` / `senseBBB()`, and the `beta`
// copy has exactly the reverse. `classes_by_signature` promises no order, which is why the probe is built
// this way rather than putting the member on one side only: whichever copy the resolver reaches first,
// exactly one of the two lookups has to survive by trying the other copy.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;

public class TwinMemberProbe {

    // The class that gets loaded twice, once patched. Nested for the same reason `TwinLoaderProbe$Widget`
    // is: `javac` emits it as an ordinary top-level class file, which a parent-less loader can define
    // against bootstrap alone.
    public static class Widget {

        static String markerAAA = "AAA-value";

        private final String owner;

        public Widget(String owner) {
            this.owner = owner;
        }

        // Reads the static, so the patched copy's method returns the patched copy's value — the two are
        // renamed together and stay wired to each other.
        public static String senseAAA() {
            return "sense " + markerAAA;
        }

        public String work(int i) {
            return owner + ":" + i + " " + markerAAA; // BP1
        }
    }

    static final String WIDGET = "TwinMemberProbe$Widget";

    // Defines `Widget` from bytes rather than delegating, so each instance owns its own copy. A
    // parent-first loader would find one `Widget` on the app classpath and hand the same type to
    // everybody, which is the case that does NOT reproduce the bug.
    static final class TwinLoader extends ClassLoader {
        private final String tag;
        private final boolean patch;

        TwinLoader(String tag, boolean patch) {
            super(null);
            this.tag = tag;
            this.patch = patch;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(WIDGET)) {
                throw new ClassNotFoundException(name);
            }
            try (InputStream in = TwinMemberProbe.class.getResourceAsStream("/" + WIDGET + ".class")) {
                if (in == null) {
                    throw new ClassNotFoundException("no " + WIDGET + ".class on the classpath");
                }
                ByteArrayOutputStream buf = new ByteArrayOutputStream();
                byte[] chunk = new byte[4096];
                for (int n; (n = in.read(chunk)) > 0; ) {
                    buf.write(chunk, 0, n);
                }
                byte[] bytes = buf.toByteArray();
                if (patch) {
                    rename(bytes, "markerAAA", "markerBBB");
                    rename(bytes, "senseAAA", "senseBBB");
                    rename(bytes, "AAA-value", "BBB-value");
                }
                return defineClass(name, bytes, 0, bytes.length);
            } catch (ClassNotFoundException e) {
                throw e;
            } catch (Exception e) {
                throw new ClassNotFoundException(name, e);
            }
        }

        // Equal-length in-place substitution. Every occurrence is rewritten, which is what keeps a field
        // and the code that reads it agreeing after the rename; unequal lengths would move every offset
        // after the entry and the class would not verify, so the names are chosen to match.
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
            return "TwinMemberLoader(" + tag + ")";
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> wa = new TwinLoader("alpha", false).loadClass(WIDGET);
        Class<?> wb = new TwinLoader("beta", true).loadClass(WIDGET);
        // The sanity check that makes everything below meaningful: two types, one name.
        System.out.println("loaded twice=" + (wa != wb) + " name=" + wa.getName());

        // Invoked here so both classes are INITIALISED before the debugger reads their statics — an
        // uninitialised class has its fields at their default values, which would read as an answer.
        System.out.println("alpha " + wa.getMethod("senseAAA").invoke(null));
        System.out.println("beta " + wb.getMethod("senseBBB").invoke(null));
        // And the negative, stated by the probe rather than assumed by the test: each copy is missing the
        // other's member, which is the whole premise.
        System.out.println("alpha lacks senseBBB=" + lacks(wa, "senseBBB"));
        System.out.println("beta lacks senseAAA=" + lacks(wb, "senseAAA"));

        Object ia = wa.getConstructor(String.class).newInstance("alpha");
        Object ib = wb.getConstructor(String.class).newInstance("beta");

        for (int i = 0; i < 100000; i++) {
            System.out.println("ran " + wa.getMethod("work", int.class).invoke(ia, i));
            System.out.println("ran " + wb.getMethod("work", int.class).invoke(ib, i));
            System.out.println("tick " + i);
            Thread.sleep(150);
        }
    }

    static boolean lacks(Class<?> c, String method) {
        try {
            c.getMethod(method);
            return false;
        } catch (NoSuchMethodException e) {
            return true;
        }
    }
}
