// Probe for BP-7 (#115) — the same class name loaded AGAIN, after a stop point is already armed.
//
//   javac -g RedeployProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8833 -cp . RedeployProbe
//
// `TwinLoaderProbe` loads one name under two loaders, but it loads BOTH before the debugger arms
// anything — so arming sees two copies and arms two copies, and the ordering this bug needs never
// happens. The sequence that costs a WildFly session is the other one, and it contains no re-arm:
//
//     set_line_stop            -> armed, on the copy that exists
//     <edit, mvn compile, cp classes, touch .dodeploy>
//     <fire the request that reaches the line>
//     get_traces               -> "No trace snapshots yet"
//
// The new deployment gets a new module classloader and defines its own copy; the old module
// classloader is still referenced by things that outlive the undeploy, so the retired copy stays
// loaded and the stop point stays armed on it. It is still listed and still enabled, so the natural
// reading of the silence is that the predicted code path is not the one running — which sends you
// back to re-read the code rather than to re-arm.
//
// The second load is on a CUE rather than a timer, because the whole point is the ordering: the test
// must arm while only `v1` exists, watch it hit, and only then deploy `v2`. A timer would race the
// arming and a green run would prove nothing.
import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.lang.reflect.Method;

public class RedeployProbe {

    // Reached only by reflection, exactly as in `TwinLoaderProbe`: naming the type anywhere in this
    // file would let the app classloader define a THIRD copy, and the stop point would arm on that.
    public static class Widget {

        private final String owner;

        public Widget(String owner) {
            this.owner = owner;
        }

        public String work(int i) {
            return owner + ":" + i; // BP1
        }
    }

    static final String WIDGET = "RedeployProbe$Widget";

    // One instance per "deployment". Parent-less, so it cannot delegate up to the app loader and
    // collapse the two deployments into one type — which is the case that does NOT reproduce the bug.
    static final class DeployLoader extends ClassLoader {
        private final String tag;

        DeployLoader(String tag) {
            super(null);
            this.tag = tag;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(WIDGET)) {
                throw new ClassNotFoundException(name);
            }
            try (InputStream in = RedeployProbe.class.getResourceAsStream("/" + WIDGET + ".class")) {
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
            return "DeployLoader(" + tag + ")";
        }
    }

    public static void main(String[] args) throws Exception {
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));

        Class<?> v1 = new DeployLoader("v1").loadClass(WIDGET);
        Object i1 = v1.getConstructor(String.class).newInstance("v1");
        Method w1 = v1.getMethod("work", int.class);
        System.out.println("deployed v1");

        // The retired deployment is kept reachable on purpose (`i1` and `w1` above). An undeployed
        // module whose loader is genuinely unreachable would eventually be collected and take its copy
        // with it; the case that costs time is the one where it is not, and the stop point goes on
        // watching a copy nothing calls.
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
                    Class<?> v2 = new DeployLoader("v2").loadClass(WIDGET);
                    System.out.println("second copy is a different type=" + (v1 != v2));
                    i2 = v2.getConstructor(String.class).newInstance("v2");
                    w2 = v2.getMethod("work", int.class);
                    System.out.println("deployed v2");
                }
            }
            System.out.println("tick " + i);
            Thread.sleep(150);
        }
    }
}
