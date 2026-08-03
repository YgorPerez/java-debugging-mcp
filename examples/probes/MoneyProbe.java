// Probe for EVAL-8 (#82) — float, double and char literals in expressions, conditions and filters.
//
//   javac -g MoneyProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8821 -cp . MoneyProbe
//
// The issue is about money, and money in the target stack is `Double` end to end. So the values here are
// not round numbers chosen for convenience — each one is a case that a debugger which merely "accepts a
// decimal point" would get wrong:
//
//   1.005   is really 1.00499999999999989341858963598497211933135986328125, which is why the target's
//           `new BigDecimal(double)` sites undercharge by a centavo. A condition `== 1.005` matching this
//           field is the whole proof that the debugger's own parser rounds the decimal string to the SAME
//           f64 javac did. If it parsed via some other route the comparison would silently never fire,
//           and a stop point that never fires is indistinguishable from code that never runs.
//   99.99   is the boundary. A `[?vlPagamento > 99.99]` filter must EXCLUDE it — an implementation that
//           was sloppy about strictness, or that compared after rounding to two decimals, passes a test
//           built only from values far from the threshold.
//   0.1f    has no exact binary form in EITHER width, and widens to 0.100000001490116119384765625 as a
//           float against 0.1000000000000000055511151231257827 as a double. So `taxa == 0.1f` is exact
//           only if the literal took the same trip through f32 that the FIELD did. A literal parsed
//           straight to f64 fails this, which is the reason ArgLit::Float holds an f32.
//
// THE PRINTS ARE THE ASSERTION, not the debugger's reply. `offer` is printed BEFORE the conditioned line
// and `charged` after it, so when a suspending condition finally matches, the probe's own stdout ends in
// an `offer` line with no `charged` line following it — naming the value the condition claimed to match.
// A condition that silently never matched leaves both lines for every value, and a condition that
// matched everything stops on the first. Neither can pass. The debugger reporting "armed" is not
// evidence, which is the reason this shape exists rather than a single print.
//
// The tick counter is on the `offer`/`charged` lines rather than a bare `tick ` line, so read it with
// `trailing_tick` — `highest_tick` keys on a line that STARTS with `tick `, returns None here, and turns
// a wait into a full-timeout failure claiming the probe froze while its output plainly shows it running.
import java.util.ArrayList;
import java.util.List;

public class MoneyProbe {

    /** One payment. Every field is a primitive the debugger had no literal for before EVAL-8. */
    static class Pagto {
        final double vlPagamento;
        final float taxa;
        final char moeda;

        Pagto(double vlPagamento, float taxa, char moeda) {
            this.vlPagamento = vlPagamento;
            this.taxa = taxa;
            this.moeda = moeda;
        }

        double getVlPagamento() {
            return vlPagamento;
        }

        /** So a filter's selection names the payment it picked rather than an identity hash. */
        @Override
        public String toString() {
            return "Pagto[" + vlPagamento + " " + taxa + " " + moeda + "]";
        }
    }

    /**
     * The cycle. Exactly one of the four is the odd one out on all three fields at once, so a condition
     * on any one of them selects the same hit and a test can cross-check them against each other.
     */
    static final double[] VALORES = {10.50, 99.99, 1.005, 1050.75};
    static final float[] TAXAS = {0.05f, 0.05f, 0.1f, 0.05f};
    static final char[] MOEDAS = {'R', 'R', 'U', 'R'};

    /** The odd value, named so a test does not have to hardcode an index. */
    static final double ODD_VALOR = 1.005;

    /** Written by the conditioned line so it cannot be optimised away. */
    static double cobrado;

    /** A list holding the whole cycle, for `[?vlPagamento > 99.99]`. Two of the four are above it. */
    static final List<Pagto> pagtos = new ArrayList<>();

    /**
     * The overload pair. `float` and `double` are DIFFERENT candidates to the JVM and scoring them apart
     * is the part of overload resolution EVAL-8 could regress: an implementation that widened every
     * floating literal to double would pick `cobrar(double)` for `2.0f` and this probe would say so.
     */
    static String cobrar(float v) {
        return "float:" + v;
    }

    static String cobrar(double v) {
        return "double:" + v;
    }

    /** A method taking a char, so `marcar('x')` proves the C tag reaches the invoke. */
    static String marcar(char c) {
        return "char:" + c;
    }

    /** A method taking a double, reached with a literal rather than an existing value. */
    static String taxar(double v) {
        return "taxa:" + v;
    }

    static void handle(Pagto p, int i) {
        System.out.println("offer " + p.vlPagamento + " taxa " + p.taxa + " moeda " + p.moeda + " tick " + i);
        cobrado += p.vlPagamento; // BP1 — a condition here reads p's fields
        System.out.println("charged " + p.vlPagamento + " tick " + i);
    }

    public static void main(String[] args) throws Exception {
        for (int k = 0; k < VALORES.length; k++) {
            pagtos.add(new Pagto(VALORES[k], TAXAS[k], MOEDAS[k]));
        }
        for (int i = 0; i < 100000; i++) {
            handle(pagtos.get(i % VALORES.length), i);
            Thread.sleep(150);
        }
    }
}
