// Probe for EVAL-11 (#98) — a static field or enum constant in ARGUMENT position, driven by
// mcp_integration.rs.
//
//   javac -g EnumArgProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8833 -cp . EnumArgProbe
//
// The capability turned out to be already present when #98 was picked up: an argument that is not a
// literal is resolved as a full expression, and that goes through the same head resolver
// `debug.evaluate` uses, so a static path resolves in argument position for free. This probe exists so
// that stays true. Nothing in the suite covered it, which is how a capability works by accident and
// then stops working by accident.
//
// The shapes that have to keep working, and what each one would break if the resolver changed:
//
//   describe(SupplierKey.OMNIBEES)        a SIMPLE-name enum constant as an argument
//   describe(EnumArgProbe.MARKER)         a `public static final` object field as an argument
//   pool[SupplierKey.OMNIBEES]            an enum constant as a Map subscript
//   describe(SupplierKey.NOPE)            the error names the missing CONSTANT, not a parse failure
//
// `describe` is deliberately OVERLOADED on the enum and on Object. An argument passed by reference has
// to be scored on its RUNTIME type, so a resolver that handed the invoke an untyped reference would
// silently pick `describe(Object)` and still look like it worked — the strings differ so the test can
// tell which ran.
//
// `SupplierKey` is a top-level type in this file rather than a nested one, on purpose: a nested enum is
// `EnumArgProbe$SupplierKey` to the JVM, so only a top-level one exercises the simple-name scan over
// loaded classes, which is the half a caller actually types.

enum SupplierKey {
    OMNIBEES,
    HOTELDO,
    EXPEDIA
}

public class EnumArgProbe {

    // A `public static final` object field, the other thing #98 is about — the ObjectMapper case in
    // it-common is exactly this shape.
    public static final String MARKER = "static-marker";

    static final java.util.Map<SupplierKey, String> pool = new java.util.HashMap<>();

    static {
        pool.put(SupplierKey.OMNIBEES, "omnibees-pool");
        pool.put(SupplierKey.HOTELDO, "hoteldo-pool");
    }

    // Overloaded on purpose — see the header.
    static String describe(SupplierKey k) {
        return "enum:" + k;
    }

    static String describe(Object o) {
        return "object:" + o;
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i + " " + describe(SupplierKey.HOTELDO)); // BP1
            Thread.sleep(150);
        }
    }
}
