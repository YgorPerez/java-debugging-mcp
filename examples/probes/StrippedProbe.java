// Probe for TEST-14 (#39): a class the JVM has loaded that carries no `SourceFile` attribute.
//
//   javac -g:none StrippedProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8804 -cp . StrippedProbe
//
// The `-g:none` on that first line is the entire point of this file, and it is the one probe the
// harness compiles that way — `Probe::launch_stripped`, never `Probe::launch`. Every other probe gets
// `-g` because without the local-variable table the expression tests silently read nothing, so no
// probe could ever reach the branch `debug.source` takes when the JVM answers `ABSENT_INFORMATION`
// (101): loaded, but compiled with no record of what it was compiled from.
//
// That branch is not an exotic edge. A vendored jar, a shaded dependency, or an app server's own
// internals routinely ship without debug info, and on the shared 8180 they are the classes someone is
// most likely to be staring at when they ask why they cannot see any code.
//
// So there is deliberately nothing here to inspect: no locals worth reading, no interesting state, no
// `// BP` marker — a `-g:none` class has no line-number table either, so a line breakpoint could not
// be placed on one anyway. It only has to be LOADED and stay alive, which is the whole precondition.
public class StrippedProbe {

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i);
            Thread.sleep(150);
        }
    }
}
