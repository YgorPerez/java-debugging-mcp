// Probe for debug.force_return.
//
//   javac -g ForceProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8802 -cp . ForceProbe
//
// The test forces `check()` to return a value its body would never produce, then reads THIS PROGRAM's
// stdout to confirm the caller actually received the forced value. That is the only proof that
// matters: force_early_return could plausibly report success to the debugger while the caller still
// sees the real return value.
//
// So every method here has a return value that is trivially predictable and never equal to what the
// test forces:
//   - check(n)  always false  → forced to true
//   - name()    always "real" → forced to "forced"
//   - count()   always 1      → forced to 99
// main() prints each result with a distinct prefix so the test can tell them apart.
public class ForceProbe {

    // Always false: n is never negative in the loop below.
    static boolean check(int n) {
        boolean result = n < 0;
        return result; // BP1
    }

    static String name() {
        String result = "real";
        return result; // BP2
    }

    static int count() {
        int result = 1;
        return result; // BP3
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            System.out.println("check=" + check(i));
            System.out.println("name=" + name());
            System.out.println("count=" + count());
            Thread.sleep(150);
        }
    }
}
