// Probe for BP-5 (#79) — one class name, two classloaders, driven by mcp_integration.rs.
//
//   javac -g TwinLoaderProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8831 -cp . TwinLoaderProbe
//
// A class name is not unique inside a JVM. Every classloader that loads a name defines its OWN
// reference type, with its own statics. `classes_by_signature` returns one entry per loader, and the
// arming path used to take `.first()` — so a stop point on a shared-library class reported "armed" and
// then watched a copy the code path in question never runs. That is indistinguishable from a wrong
// hypothesis about the code, which is what makes it worse than a missing feature.
//
// This is not exotic on the target stack. WildFly gives every deployment its own module classloader,
// `it-common` and `api-common` are packed into each consuming war's WEB-INF/lib with no shared module,
// and `infotravel.war` and `integraws.war` are deliberately co-deployed into the same JVM. So
// `br.com.infotera.common.util.Utils` genuinely exists twice, and its public static state — the
// airport map, the environment flag, the endpoint URLs — is a different object per war.
//
// Reproduced here with two loaders that DEFINE the class from bytes instead of delegating. Each has
// `null` as its parent, so neither can delegate up to the app loader and collapse the two into one
// type. `Widget` therefore loads twice and `Widget.work()` — the marked line — exists as two distinct
// methods in two distinct reference types with the same name.
//
// The probe drives BOTH copies every tick and prints which one ran, because the debugger reports
// success either way: the probe's own stdout is the only evidence both copies actually executed.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;

public class TwinLoaderProbe {

    // The class that gets loaded twice.
    //
    // A static nested class rather than its own file only because the test harness compiles one source
    // file per probe; `javac` emits it as `TwinLoaderProbe$Widget.class`, which is an ordinary top-level
    // class file as far as a classloader is concerned. It touches nothing outside itself, so a loader
    // with no parent can define it against bootstrap alone.
    public static class Widget {
        // A static, so the two copies visibly hold different state — the `Utils.tpAmbiente` shape that
        // makes reading the wrong copy an actively wrong answer rather than a slow one.
        static int calls = 0;

        private final String owner;

        public Widget(String owner) {
            this.owner = owner;
        }

        public String work(int i) {
            calls++;
            return owner + ":" + i + " calls=" + calls; // BP1
        }
    }

    static final String WIDGET = "TwinLoaderProbe$Widget";

    // Defines `Widget` from bytes rather than delegating, so each instance of this loader owns its own
    // copy. A parent-first loader would find one `Widget` on the app classpath and hand the same type to
    // everybody, which is the case that does NOT reproduce the bug.
    static final class TwinLoader extends ClassLoader {
        private final String tag;

        TwinLoader(String tag) {
            super(null);
            this.tag = tag;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(WIDGET)) {
                throw new ClassNotFoundException(name);
            }
            try (InputStream in = TwinLoaderProbe.class.getResourceAsStream("/" + WIDGET + ".class")) {
                if (in == null) {
                    throw new ClassNotFoundException("no " + WIDGET + ".class on the classpath");
                }
                ByteArrayOutputStream buf = new ByteArrayOutputStream();
                byte[] chunk = new byte[4096];
                for (int n; (n = in.read(chunk)) > 0; ) {
                    buf.write(chunk, 0, n);
                }
                byte[] bytes = buf.toByteArray();
                return defineClass(name, bytes, 0, bytes.length);
            } catch (ClassNotFoundException e) {
                throw e;
            } catch (Exception e) {
                throw new ClassNotFoundException(name, e);
            }
        }

        @Override
        public String toString() {
            return "TwinLoader(" + tag + ")";
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> wa = new TwinLoader("alpha").loadClass(WIDGET);
        Class<?> wb = new TwinLoader("beta").loadClass(WIDGET);
        // The sanity check that makes everything below meaningful: two types, one name.
        System.out.println("loaded twice=" + (wa != wb) + " name=" + wa.getName());

        Object ia = wa.getConstructor(String.class).newInstance("alpha");
        Object ib = wb.getConstructor(String.class).newInstance("beta");

        for (int i = 0; i < 100000; i++) {
            System.out.println("ran " + wa.getMethod("work", int.class).invoke(ia, i));
            System.out.println("ran " + wb.getMethod("work", int.class).invoke(ib, i));
            System.out.println("tick " + i);
            Thread.sleep(150);
        }
    }
}
