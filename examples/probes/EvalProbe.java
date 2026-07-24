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
