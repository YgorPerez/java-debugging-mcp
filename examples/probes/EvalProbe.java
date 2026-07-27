// Probe for EVAL-1 (static-method invocation) + EVAL-2 (object arguments in method calls),
// driven by examples/test_eval_invoke.rs. Loops calling work(...) so a breakpoint there is easy to
// hit; work() is static on purpose, so `this` is unavailable and object arguments must come from
// locals or static fields.
//
//   javac -g EvalProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8793 -cp . EvalProbe
public class EvalProbe {

    static String infra = "PROD";
    static int base = 7;

    // --- DISC-5 (#53): the three shapes debug.list_fields has to tell apart, in one class ---
    // A `static final` (the marker that says a set_value may be refused and a field stop can never
    // fire), and the probe's only INSTANCE field — EvalProbe is otherwise all statics, so without this
    // there is nothing here to prove that statics are listed first.
    static final int LIMIT = 3;
    int seq;

    // --- static methods for EVAL-1 ---
    public static int twice(int n) { return n * 2; }
    public static String greet(String who) { return "hello " + who; }
    public static int sum(int a, int b) { return a + b; }
    public static String infraName() { return infra; }

    // --- static methods taking objects, for EVAL-2 ---
    public static String describe(Item it) { return "item:" + it.name + "/" + it.qty; }
    public static boolean sameName(Item a, Item b) { return a.name.equals(b.name); }

    // --- overloads that are indistinguishable by JDWP tag alone (both 'L') ---
    public static String pick(String s) { return "String:" + s; }
    public static String pick(Item i) { return "Item:" + i.name; }
    public static String pick(Object o) { return "Object"; }

    // instance method that must NOT be picked for a static call of the same name/arity
    public String pick(int n) { return "instance"; }

    // --- EVAL-3: parameters whose match can't be read off the superclass chain ---
    // An interface-typed parameter never appears in an argument's superclass chain, so matching one
    // means asking the JVM (ReferenceType.Interfaces, walked transitively).
    public static String takesRunnable(Runnable r) { return "Runnable"; }
    public static String takesComparable(Comparable<?> c) { return "Comparable"; }
    // Only reachable by autoboxing an int argument. Note it is NOT overloaded with an int version:
    // Java itself would prefer the primitive, so an overload pair here would test the wrong thing.
    public static String takesInteger(Integer boxed) { return "Integer:" + boxed; }
    // A concrete class with no relation to the arguments above — the negative case that must be
    // REJECTED rather than picked by a blind arity/kind fallback.
    public static String takesThread(Thread t) { return "Thread"; }
    // Array covariance: a String[] is an Object[], which no signature comparison can tell you.
    public static String takesObjects(Object[] xs) { return "Object[]:" + xs.length; }

    // Implements one interface directly and inherits another through its superclass, so the
    // transitive walk has something to find that a direct-superinterface query would miss.
    public static class Task implements Runnable {
        // DISC-5 (#53): the one field a subclass INHERITS, so the field listing's superclass walk has
        // something to attribute — and the probe's only `volatile`, the third modifier it marks.
        volatile int runs;
        @Override public void run() { }
    }

    public static class Subtask extends Task { }

    public static class Item {
        String name;
        int qty;
        Item(String n, int q) { name = n; qty = q; }
        boolean matches(Item o) { return o != null && name.equals(o.name); }
        int plus(int d) { return qty + d; }
        String label() { return name + "#" + qty; }
        @Override public String toString() { return "Item(" + name + "," + qty + ")"; }
    }

    static Item holder = new Item("holder", 3);
    static Task task = new Task();
    static Subtask subtask = new Subtask();
    static String[] words = {"alpha", "beta"};

    // The BP<n> markers are how test_eval_invoke.rs finds its breakpoint lines — it greps this file
    // rather than hardcoding numbers, so editing above here is safe. Keep one marker per line.
    static void work(Item a, Item b, int n) {
        int local = n;
        System.out.println("work " + local + " " + a + " " + b); // BP1: locals a, b, n, local
        int check = local * 2; // BP2: a and b live, `check` not yet assigned
        if (check < 0) { System.out.println(check); } // BP3: `check` == local * 2
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            work(new Item("alpha", 1), new Item("alpha", 2), i);
            Thread.sleep(150);
        }
    }
}
