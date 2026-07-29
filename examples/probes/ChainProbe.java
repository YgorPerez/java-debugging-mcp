// Probe for EVAL-6 (#70) — a chained expression whose value goes null partway down.
//
//   javac -g ChainProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8815 -cp . ChainProbe
//
// Shaped after the chain the investigation behind #67 actually bisected by hand, one `debug.evaluate` at
// a time:
//
//   wsReservaCircuito.getCircuitoParametro()                  → object
//   wsReservaCircuito.getCircuitoParametro().getConfigUhList() → ArrayList
//   wsReservaCircuitoUh.getSqQuarto()                          → null
//
// Two different failures, because they read differently and a walk has to handle both:
//
//   - `getSqQuarto()` returns null at the END of a chain that otherwise resolves. Every link before it is
//     fine, and the question is only which one stopped being useful.
//   - `getMissing()` returns null in the MIDDLE, so the links after it can never be evaluated at all.
//     That is the case where a plain `debug.evaluate` reports a null receiver — the question restated
//     rather than answered.
//
// Nothing here throws, which is the whole point: #67 covers the throwing case, since a JDK 15+ helpful
// NPE names the failing subexpression itself.
public class ChainProbe {

    // The null this exists to find, one level below a collection element — deliberately an `Integer`
    // rather than an `int`, because a primitive cannot be null and there would be nothing to look for.
    public static class Leaf {
        Integer sqQuarto = null;

        Integer getSqQuarto() {
            return sqQuarto;
        }
    }

    public static class Config {
        java.util.List<Leaf> uhList = new java.util.ArrayList<>();

        Config() {
            uhList.add(new Leaf());
        }

        java.util.List<Leaf> getConfigUhList() {
            return uhList;
        }
    }

    public static class Parametro {
        Config config = new Config();

        Config getConfig() {
            return config;
        }
    }

    public static class Reserva {
        Parametro parametro = new Parametro();
        // The mid-chain null: everything hangs off this and it is never set.
        Parametro missing = null;

        Parametro getCircuitoParametro() {
            return parametro;
        }

        Parametro getMissing() {
            return missing;
        }
    }

    static void inspect(Reserva reserva, int i) {
        System.out.println("tick " + i + " ready"); // BP_CHAIN — `reserva` is in scope here
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            inspect(new Reserva(), i);
            Thread.sleep(150);
        }
    }
}
