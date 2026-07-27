// Probe for TEST-15 (#40): a class whose real source is not the `.java` it was compiled from.
//
//   javac -g SmapProbe.java
//   # …then install SmapProbe.smap into SmapProbe.class — see Probe::launch_with_smap
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8805 -cp . SmapProbe
//
// This stands in for the JSP case, which is the reason the SMAP path exists at all: on the shared 8180,
// Jasper compiles `hello.jsp` into a `_jsp.java` servlet, compiles THAT, and records a JSR-45 SMAP in the
// class saying which lines of the `.jsp` each generated line came from. Ask such a class what it was
// compiled from and it answers with the generated file, truthfully and uselessly — the file someone
// actually wrote is named only in the SMAP.
//
// `javac` has no option that emits a `SourceDebugExtension`, so there is no Java that can be written here
// to produce one. The attribute is spliced into the compiled class file afterwards, which is exactly what
// Jasper itself does (`SmapUtil$SDEInstaller`). The Java below therefore looks entirely ordinary, and that
// is the point: what distinguishes it lives in the class file, not in the source.
//
// `Neighbour` is the control, and the reason it is in this file rather than its own: it comes out of the
// SAME `javac` invocation, from the SAME source file, and does NOT get the attribute. A test that only
// looked at the patched class would pass just as happily if `debug.source` announced an SMAP for
// everything; this pins the difference to the one class that carries one.
public class SmapProbe {

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            Neighbour.touch();
            System.out.println("tick " + i + " " + Neighbour.touched);
            Thread.sleep(150);
        }
    }
}

// Loaded on the first tick, so `debug.source` can be asked about it. A class the debuggee never loads is
// indistinguishable from one that does not exist.
class Neighbour {
    static int touched = 0;

    static void touch() {
        touched++;
    }
}
