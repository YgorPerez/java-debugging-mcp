// Probe for DISC-12 (#95) — generic type information, and the fallback when there is none.
//
//   javac -g GenericsProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8824 -cp . GenericsProbe
//
// The point of the issue is composing the NEXT expression: look at a frame, see a `List`, and guess what is
// in it. So this probe is built around the cases where guessing is what a caller was reduced to, plus the
// cases where a generic signature does not exist at all — because that fallback is the whole design risk.
// A generic signature is an OPTIONAL class-file attribute and the JDWP generic commands answer with an
// EMPTY STRING when there is none, so a naive implementation renders a blank type where the raw descriptor
// used to be correct. Every `raw*` member below has no `Signature` attribute and must render exactly what it
// rendered before DISC-12.
//
// `words` and `grid` are the pair that catches a parser confusing an array of a parameterised type with a
// parameterised type of an array. `bounded` is the wildcard case the acceptance criteria name. `sessions`
// is the shape the issue calls the worst found in the target: two levels of Map, which reads as nothing
// navigable when rendered raw.
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;

public class GenericsProbe {

    /** An element type worth naming, so `List<Widget>` is visibly better than `List`. */
    public static class Widget {
        int qty = 3;
    }

    /** Stands in for the target's `WSSessao` — see `sessions` below. */
    public static class WSSessao {
        String schema = "infotravel";
    }

    // ---- members that DO carry a generic signature ----

    public static List<String> names = new ArrayList<>();
    public static Map<Integer, List<Widget>> byId = new HashMap<>();
    /** The worst case in the issue: `Map<Integer, Map<WSIntegradorEnum, LinkedList<WSSessao>>>`. */
    public static Map<Integer, Map<String, LinkedList<WSSessao>>> sessions = new HashMap<>();
    /** A wildcard, which the acceptance criteria require not be mangled. */
    public static Map<String, ? extends Number> bounded = new HashMap<String, Integer>();
    /** An ARRAY of a parameterised type — not a parameterised type of an array. */
    public static List<String>[] buckets = new List[2];

    // ---- members that carry NO generic signature, and must render exactly as before ----

    /** A RAW `List`. `javac` emits no `Signature` attribute for this, which is the point of it. */
    @SuppressWarnings("rawtypes")
    public static List rawList = new ArrayList();
    public static int rawQty = 7;
    public static String rawName = "plain";
    public static Widget[] rawWidgets = new Widget[] {new Widget()};
    public static long[][] rawGrid = new long[2][2];

    // ---- methods ----

    /** A generic method: its own type parameter, a parameterised argument and a parameterised return. */
    public static <T> List<T> firstOf(List<T> input, Map<String, T> lookup) {
        return input;
    }

    /** A bounded type parameter, so `<T extends Number & Comparable<T>>` has to render. */
    public static <T extends Number & Comparable<T>> T biggest(List<T> input) {
        return input.get(0);
    }

    /** NO generic signature at all — the control, which must render byte-for-byte what it always did. */
    public static int twiceRaw(int n) {
        return n * 2;
    }

    /**
     * The frame a test reads. Every local here is declared with its type arguments, so `debug.get_stack`
     * has something to show that the value alone cannot say — `lines` renders as an `ArrayList` either way,
     * and `List<Widget>` is the part that lets a caller write `lines[0].qty` without guessing.
     *
     * `plainCount` and `plainText` are in the same frame on purpose: they have no generic signature, so a
     * test can assert in one reply that they are UNCHANGED while the others gained a type.
     */
    static void inspect(int i) {
        List<Widget> lines = new ArrayList<>();
        lines.add(new Widget());
        Map<String, List<Widget>> grouped = new HashMap<>();
        grouped.put("a", lines);
        int plainCount = i;
        String plainText = "unchanged";
        System.out.println("tick " + i + " ready"); // BP1 — every local above is in scope here
    }

    public static void main(String[] args) throws Exception {
        names.add("first");
        byId.put(1, new ArrayList<>());
        sessions.put(1, new HashMap<>());
        rawList.add("raw");
        for (int i = 0; i < 100000; i++) {
            inspect(i);
            Thread.sleep(150);
        }
    }
}
