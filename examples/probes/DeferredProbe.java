// Probe for the deferred ("class not loaded yet") breakpoint path.
//
//   javac -g DeferredProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8801 -cp . DeferredProbe
//
// The whole point is that `LateWorker` must NOT be loaded when the debugger sets its breakpoint —
// otherwise the ordinary already-loaded path is taken and the deferred code never runs. So the class
// is reached only by reflection, and only after a line arrives on stdin. The test therefore controls
// the ordering exactly: attach → set breakpoint on an unloaded class → send the cue → the JVM loads
// the class, ClassPrepare fires, the pump arms the real breakpoint, and it hits.
//
// Referencing LateWorker directly anywhere in this file would let the verifier load it eagerly, which
// is why `Class.forName` is used instead.
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.lang.reflect.Method;

public class DeferredProbe {

    public static void main(String[] args) throws Exception {
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));
        System.out.println("ready");

        // Idle until told to load. Printing a heartbeat proves the JVM is alive and not suspended.
        String line;
        while ((line = in.readLine()) != null) {
            if ("load".equals(line.trim())) {
                break;
            }
            System.out.println("waiting");
        }

        Class<?> cls = Class.forName("LateWorker");
        // getDeclaredMethod, not getMethod: LateWorker is package-private (only one public top-level
        // class per file), and getMethod would not find its method.
        Method work = cls.getDeclaredMethod("work", int.class);
        // Loop so the breakpoint has many chances to hit, and so the test can confirm the JVM keeps
        // running after a resume.
        for (int i = 0; i < 100000; i++) {
            Object result = work.invoke(null, i);
            System.out.println("late " + result);
            Thread.sleep(150);
        }
    }
}

class LateWorker {
    static String label = "late-worker";

    public static int work(int n) {
        int doubled = n * 2;
        return doubled; // BP1: the deferred breakpoint lands here
    }
}
