// Probe for BP-4 (re-arm re-resolves by name), driven by mcp_integration.rs.
//
//   javac -g ReloadProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8813 -cp . ReloadProbe
//
// BP-4 exists because a JDWP referenceTypeID / methodID / fieldID is only valid while that type stays
// loaded, and the realistic sequence on a shared app server is "disable the breakpoint, redeploy, re-arm
// it". A WildFly redeploy discards the old classloader and loads the same class afresh, so every cached id
// for it goes stale.
//
// This reproduces that SHAPE locally, in the same spirit as MetricsProbe standing in for Micrometer: the
// worker class is loaded through a throwaway URLClassLoader, exercised, then dropped and loaded again
// through a NEW loader. The second copy is a genuinely different reference type with different ids, which is
// the condition a cached-id re-arm gets wrong. What it does NOT reproduce is WildFly's own machinery —
// module classloaders, deployment lifecycle, hundreds of threads — so the real-instance check
// (issue #13) still stands.
//
// The reloadable worker is emitted as source and compiled at runtime, so this file stays self-contained
// and there is no second .java for the harness to know about.
import java.io.File;
import java.io.FileWriter;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;

public class ReloadProbe {

    // Bumped by the reloadable worker on every call, through a static on THIS class (which never
    // reloads), so the tick keeps advancing across a reload and a test can prove the probe kept running.
    public static int counter = 0;

    public static void bump() {
        counter = counter + 1;
    }

    /** Source of the class that gets loaded, dropped, and loaded again. */
    private static final String WORKER_SRC =
        "public class Worker {\n"
        + "    public static int calls = 0;\n"
        + "    public void work() {\n"
        + "        calls = calls + 1;\n"          // Worker.calls — a field a watchpoint can target
        + "        ReloadProbe.bump();\n"         // WORKER_BP: the line a breakpoint targets
        + "    }\n"
        + "}\n";

    /** Compile Worker.java into a fresh directory and return a loader over it. */
    private static ClassLoader freshLoader(Path dir, int generation) throws Exception {
        Path gen = dir.resolve("gen" + generation);
        Files.createDirectories(gen);
        File src = gen.resolve("Worker.java").toFile();
        try (FileWriter w = new FileWriter(src)) {
            w.write(WORKER_SRC);
        }
        // -g so the reloaded copy has a line-number table too; without it a line breakpoint on the
        // reloaded class could not resolve at all and the test would be measuring the wrong failure.
        int rc = javax.tools.ToolProvider.getSystemJavaCompiler()
            .run(null, null, null, "-g", "-d", gen.toString(), src.getAbsolutePath());
        if (rc != 0) {
            throw new IllegalStateException("failed to compile Worker generation " + generation);
        }
        // Parent-last is not needed; Worker only references ReloadProbe, which the app loader owns.
        return new URLClassLoader(new URL[]{ gen.toUri().toURL() }, ReloadProbe.class.getClassLoader());
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("reloadprobe");

        int generation = 0;
        ClassLoader loader = freshLoader(dir, generation);
        Class<?> workerClass = loader.loadClass("Worker");
        Object worker = workerClass.getDeclaredConstructor().newInstance();
        Method work = workerClass.getMethod("work");

        // A reload every 40 ticks (~6s at 150ms). Frequent enough that a test doesn't wait long, rare
        // enough that a breakpoint armed on the current generation gets many hits before it goes away.
        for (int i = 0; i < 100000; i++) {
            work.invoke(worker);
            System.out.println("tick " + counter + " gen " + generation);
            Thread.sleep(150);

            if (i > 0 && i % 40 == 0) {
                // The "redeploy": drop the old loader and its Worker, load a brand-new one. The previous
                // reference type becomes unreachable, so every JDWP id cached for it is now stale.
                generation++;
                if (loader instanceof URLClassLoader) {
                    ((URLClassLoader) loader).close();
                }
                loader = freshLoader(dir, generation);
                workerClass = loader.loadClass("Worker");
                worker = workerClass.getDeclaredConstructor().newInstance();
                work = workerClass.getMethod("work");
                System.gc(); // encourage the old type to actually go away
                System.out.println("reloaded gen " + generation);
            }
        }
    }
}
