// Probe for FILT-9 (InstanceOnly filters), driven by mcp_integration.rs.
//
//   javac -g InstProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8815 -cp . InstProbe
//
// Two live instances of one class, `X` and `Y`, driving every stop point kind that can carry an
// `InstanceOnly` modifier. The point of the probe is that both instances do the same work in the same
// loop, so a filter pinned to one of them is only correct if the records name that one and no other —
// and a filter the JVM *accepted but did not apply* shows up as records from both.
//
// Four shapes, deliberately paired so each has an unfiltered twin:
//
//   work()      instance method, instance field write   — `this` exists, and the filter applies
//   stat()      static method, static field write       — no `this`, so there is nothing to filter on
//   boom()      instance method that throws and catches — `this` exists at the throw
//   name/touched vs statics — the field watch pair, for the same reason
//
// `boom()` is the one that decided an open question rather than confirming a measured one: HotSpot
// accepts an `InstanceOnly` modifier on an `EXCEPTION` request, and whether it then *applies* it is not
// something the reply, the capability bits or the spec will tell you. Both instances throw the same
// custom exception type from the same line, one after the other, every iteration — so a single run
// separates "filtered to X" from "accepted and ignored" by whether any record names the twin. (It
// applies. Temurin 17/21/25, 26 records, all of them the filtered instance.)
//
// A custom exception type, not a JDK one, so an internal JVM throw cannot be mistaken for a hit.
//
// ## The `drop` cue
//
// `Y` is droppable on demand, so a filter can be watched losing its object while it is armed. A JDWP
// object id is a WEAK reference (ADR-0022): once the debuggee collects the object the filter stops
// matching and the stop point goes quiet, which is indistinguishable from the code never running.
//
// A separate `gc` cue runs another two collections on demand. It is not redundant with `drop`: when a
// debugger-side filter pins `Y`, the collections `drop` runs are precisely the ones that must fail to
// free it, so showing that releasing the filter released the object needs a collection AFTER that — and
// an unreachable object nobody asks about is simply never collected.
//
// `Y` is therefore a **static volatile field read straight into each call**, never a local. A local in
// `main`'s frame is a GC root whether or not the code will read it again, so `InstProbe y = Y;` would
// keep the object alive across every `System.gc()` and the drop would silently do nothing — the same
// trap `CapturedProbe` documents, and it is worth repeating here because it fails by *passing*.
//
// ## The heartbeat
//
// `alive N` every iteration, for the reason TRACE-8's follow-up (#114) exists: a probe that has stopped
// generating the behaviour under test must say so, or every assertion downstream is measuring the probe
// rather than the debugger. A frozen worker and a working filter look identical from outside.
public class InstProbe {

    static class Boom extends RuntimeException {
        Boom(String message) {
            super(message);
        }
    }

    static int statics = 0;
    int touched = 0;
    final String name;

    InstProbe(String n) {
        name = n;
    }

    // Never a local: see the class comment. `X` is held for the life of the run; `Y` is what the `drop`
    // cue clears.
    static volatile InstProbe x = new InstProbe("X");
    static volatile InstProbe y = new InstProbe("Y");
    static volatile boolean dropRequested = false;
    static volatile boolean gcRequested = false;

    // Instance method, instance field write. Measured: InstanceOnly applies to both the line stop and
    // the field watch here.
    void work() {
        touched++;
        System.out.println("work " + name + " " + touched); // BP1
    }

    // Static method, static field write. No `this`, so there is nothing for the filter to match and
    // HotSpot fires for every hit — accepted, not applied. Both shapes are refused up front now.
    static void stat() {
        statics++; // BP2
    }

    // Instance method that throws and swallows. The `this` at the throw is the instance, so this is the
    // shape an InstanceOnly filter on an EXCEPTION request scopes — and does scope, measured.
    void boom() {
        try {
            throw new Boom("boom " + name); // BP3
        } catch (Boom e) {
            touched += 0;
        }
    }

    // Clear the last strong reference to `Y` and make the collection happen, rather than waiting for a
    // pool to do it at a time no test can predict.
    static void dropAndCollect() throws InterruptedException {
        y = null;
        collect();
        System.out.println("dropped");
    }

    // A SECOND, repeatable collection, and the reason it exists is the whole `InstanceOnly` pin
    // measurement. A debugger-side filter holds the object, so the collections `drop` runs are the ones
    // that must NOT free it; proving the pin was released then needs another collection afterwards, and
    // an unreachable object that nobody asks to collect simply stays uncollected. Without this the test
    // would report "still pinned" for a JVM that had merely not been asked again — a false negative that
    // looks exactly like the finding.
    static void collect() throws InterruptedException {
        for (int i = 0; i < 2; i++) {
            System.gc();
            Thread.sleep(150);
        }
    }

    static void readCues() {
        Thread t = new Thread(() -> {
            try (java.io.BufferedReader r =
                    new java.io.BufferedReader(new java.io.InputStreamReader(System.in))) {
                String line;
                while ((line = r.readLine()) != null) {
                    if (line.contains("drop")) {
                        dropRequested = true;
                    } else if (line.contains("gc")) {
                        gcRequested = true;
                    }
                }
            } catch (Exception e) {
                System.out.println("cue reader stopped: " + e);
            }
        });
        t.setDaemon(true);
        t.start();
    }

    public static void main(String[] a) throws Exception {
        readCues();
        System.out.println("ready");
        boolean dropped = false;
        for (int i = 0; i < 100000; i++) {
            // Read out of the field into the call with no local to hold it: see dropAndCollect.
            x.work();
            if (y != null) {
                y.work();
            }
            stat();
            x.boom();
            if (y != null) {
                y.boom();
            }
            if (dropRequested && !dropped) {
                dropped = true;
                dropAndCollect();
            }
            if (gcRequested) {
                gcRequested = false;
                collect();
                System.out.println("collected");
            }
            System.out.println("alive " + i);
            Thread.sleep(150);
        }
    }
}
