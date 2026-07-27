// Probe for TEST-10 (#35): every Java primitive, and an array of every Java primitive, as locals, as
// static fields and as instance fields.
//
//   javac -g PrimitiveProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8826 -cp . PrimitiveProbe
//
// `jdwp-client/src/types.rs` measured 16.67% region coverage, and the coverage review's verdict was "one
// big match over value kinds and most arms are for types the probes never produce — low percentage, not a
// finding". That is true *because of the probes*: `EvalProbe` and the rest deal in `int`, `String` and
// objects, so `byte`, `short`, `char`, `float` and `boolean` had never once come back over the wire in a
// test. A renderer nobody has ever run is not a renderer anyone knows the output of.
//
// The **arrays are the load-bearing half**, and not for symmetry. `handlers.rs` renders a bare primitive
// with its own private copy of the match (`render_primitive`); the copy in `types.rs` — `Value::format` —
// is reached only for ARRAY ELEMENTS and for the type-mismatch message. So a probe with eight primitive
// locals and no arrays would exercise the duplicate and leave the original exactly as unmeasured as
// before.
//
// Values are chosen so the rendering is checkable rather than merely present:
//
//   * Signed extremes (`-7`, `-300`, `Integer.MIN_VALUE`, a long past 2^32) catch a width or a
//     signedness mistake, which the usual small positive test value cannot: 3 renders as 3 whether it
//     was read as a byte, a short or an int.
//   * Floats are exact binary fractions (1.5, 0.5, -1.25, 2.5, -0.125), so a test can pin the exact
//     string without pinning a formatting library's rounding.
//   * `chars[2]` is a LONE SURROGATE, `(char) 0xD800`. A Java `char` is a UTF-16 code unit and not a
//     Unicode scalar value, so half of a surrogate pair is a perfectly ordinary thing to find in a
//     `char[]` — and it is exactly the input the renderer's `unwrap_or('?')` fallback used to swallow,
//     rendering it byte-for-byte as a real question mark until TYPE-1 (#48) made it say what it is. Written
//     as a numeric cast rather than a `'\uD800'` escape, because Java processes unicode escapes before
//     lexing and the escape form is a needless way to depend on that.
//
// The same eight values appear three ways on purpose: as `work`'s parameters and locals (read through
// `debug.get_stack`), as static fields, and as fields of an instance (both read through
// `debug.evaluate`). Those are three different resolution paths in the handlers, and the point is that
// all three end at the same rendering.
public class PrimitiveProbe {

    // --- static fields ---
    static byte sByte = -7;
    static short sShort = -300;
    static char sChar = 'Q';
    static int sInt = -2147483648;
    static long sLong = 9000000000L;
    static float sFloat = 1.5f;
    static double sDouble = -2.25;
    static boolean sBoolean = true;

    static byte[] sBytes = {1, -2, 127};
    static short[] sShorts = {-300, 0, 300};
    static char[] sChars = {'a', 'Z', (char) 0xD800};
    static int[] sInts = {0, -1, 2147483647};
    static long[] sLongs = {-9000000000L, 9000000000L};
    static float[] sFloats = {0.5f, -1.25f};
    static double[] sDoubles = {2.5, -0.125};
    static boolean[] sBooleans = {true, false};

    /// The same eight, and their arrays, as INSTANCE fields — a different lookup path from the statics
    /// above, ending at the same renderer.
    public static class Holder {
        byte b = -7;
        short s = -300;
        char c = 'Q';
        int i = -2147483648;
        long j = 9000000000L;
        float f = 1.5f;
        double d = -2.25;
        boolean z = true;

        byte[] bs = {1, -2, 127};
        short[] ss = {-300, 0, 300};
        char[] cs = {'a', 'Z', (char) 0xD800};
        int[] is = {0, -1, 2147483647};
        long[] js = {-9000000000L, 9000000000L};
        float[] fs = {0.5f, -1.25f};
        double[] ds = {2.5, -0.125};
        boolean[] zs = {true, false};
    }

    static Holder holder = new Holder();

    /// Every primitive as a parameter and every primitive array as a local, all in scope at BP1.
    ///
    /// The `println` is not decoration: a local javac can prove is never read may carry no useful range
    /// in the local-variable table, and a local the JVM will not report is a local this probe did not
    /// actually present.
    static void work(byte b, short s, char c, int i, long j, float f, double d, boolean z) {
        byte[] bs = {1, -2, 127};
        short[] ss = {-300, 0, 300};
        char[] cs = {'a', 'Z', (char) 0xD800};
        int[] is = {0, -1, 2147483647};
        long[] js = {-9000000000L, 9000000000L};
        float[] fs = {0.5f, -1.25f};
        double[] ds = {2.5, -0.125};
        boolean[] zs = {true, false};
        System.out.println("work " + b + " " + s + " " + c + " " + i + " " + j + " " + f + " " + d
                + " " + z + " " + bs.length + ss.length + cs.length + is.length + js.length
                + fs.length + ds.length + zs.length);
        // The marker sits on a statement of its own, and on ONE line. A `// BP<n>` on the last line of a
        // statement that spans several is a line the compiler emitted no code for, so the breakpoint
        // arms against nothing and the test times out saying only "never fired".
        touched = zs.length; // BP1: every primitive and every primitive array above is in scope here
    }

    /// Somewhere for the marker statement to write, so it cannot be optimised away.
    static int touched;

    public static void main(String[] args) throws Exception {
        for (int n = 0; n < 100000; n++) {
            work(sByte, sShort, sChar, sInt, sLong, sFloat, sDouble, sBoolean);
            System.out.println("tick " + n);
            Thread.sleep(150);
        }
    }
}
