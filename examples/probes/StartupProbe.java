// Probe for LAUNCH-1: code that runs BEFORE anything a debugger could attach to.
//
//   javac -g StartupProbe.java
//   # started by debug.launch, not by hand — that is the point
//
// A static initialiser is the sharpest case for `suspend=y`. By the time you can attach to a JVM someone
// else started, `<clinit>` has already run and the only evidence left is its effect. A JVM launched with
// suspend=y has not loaded this class yet, so a breakpoint inside the initialiser is still ahead of the
// program — which is the one thing attaching can never offer.
public class StartupProbe {

    static int computed;

    static {
        computed = 6 * 7; // BP_CLINIT: unreachable unless the debugger got here before class initialisation
        System.out.println("clinit " + computed);
        System.out.flush();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("ready " + computed);
        System.out.flush();
        // Idle so the test can inspect a live JVM rather than racing its exit.
        for (int i = 0; i < 100000; i++) {
            Thread.sleep(200);
        }
    }
}
