// Probe for FILT-3: ONE wildcard pattern arming a breakpoint on several classes at once — including a
// class that only loads later, and a class that matches the pattern but has nothing to do with it.
//
//   javac -g FamilyProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8801 -cp . FamilyProbe
//
// Four classes match `Family*`, and each is here to make one thing testable:
//
//   FamilyAlpha, FamilyBeta   loaded before the debugger arms anything, both declaring `handle` — so a
//                             wildcard must arm TWO breakpoints from one call.
//   FamilyNoMethod            loaded, matches the pattern, declares no `handle` — the case a broad
//                             pattern produces most of, which must be counted and NOT reported as an
//                             error (`FamilyProbe` itself is a second instance of it).
//   FamilyGamma               reached only by reflection, and only after a line arrives on stdin — so
//                             the family's `CLASS_PREPARE` watch is what arms it, minutes after the
//                             call that created the family. Referencing it directly anywhere in this
//                             file would let the verifier load it eagerly and the test would prove
//                             nothing.
//
// The worker thread keeps calling all of them so a traced (non-suspending) family has something to
// record, and so the test can tell "armed" from "armed and firing".
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.lang.reflect.Method;

public class FamilyProbe {

    // Written by the main thread after the cue, read by the worker — volatile because the worker must
    // see it without any synchronisation the debugger could be holding.
    static volatile Object gamma = null;
    static volatile Method gammaHandle = null;

    public static void main(String[] args) throws Exception {
        // Loaded HERE, before "ready" is printed, so they are already loaded when the debugger arms.
        final FamilyAlpha alpha = new FamilyAlpha();
        final FamilyBeta beta = new FamilyBeta();
        final FamilyNoMethod none = new FamilyNoMethod();

        Thread worker = new Thread(() -> {
            int i = 0;
            while (true) {
                try {
                    alpha.handle(i);
                    beta.handle(i);
                    none.other();
                    Method m = gammaHandle;
                    if (m != null) {
                        m.invoke(gamma, i);
                    }
                    Thread.sleep(100);
                } catch (Exception e) {
                    return;
                }
                i++;
            }
        });
        worker.setDaemon(true);

        System.out.println("ready");
        System.out.flush();
        worker.start();

        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));
        String line;
        while ((line = in.readLine()) != null) {
            if ("load".equals(line.trim())) {
                Class<?> cls = Class.forName("FamilyGamma");
                gamma = cls.getDeclaredConstructor().newInstance();
                gammaHandle = cls.getDeclaredMethod("handle", int.class);
                System.out.println("gamma loaded");
                System.out.flush();
            }
        }
    }
}

class FamilyAlpha {
    public int handle(int n) {
        int r = n + 1;
        return r; // BP_HANDLE: the first line of handle, where a wildcard family lands
    }
}

class FamilyBeta {
    public int handle(int n) {
        int r = n + 2;
        return r;
    }
}

// Loads only on the cue, by reflection — the family's watch has to arm this one.
class FamilyGamma {
    public int handle(int n) {
        int r = n + 3;
        return r;
    }
}

// Matches `Family*` and has no `handle`: not a target, not an error.
class FamilyNoMethod {
    public int other() {
        return 7;
    }
}
