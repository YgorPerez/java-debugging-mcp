// Probe for EVAL-7 (#81): a byte[] read as text, under the right charset and under the wrong one, and
// `array.length`.
//
//   javac -g -encoding UTF-8 BytesProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8831 -cp . BytesProbe
//
// The shape being reproduced is `it-common`'s `WSIntegradorLog`, which holds `private byte[] dsRequest`
// and `private byte[] dsResponse` and is where every supplier round trip on the shared 8180 ends up. It
// was readable only as a bag of signed integers, and the two facts that made it worse than tedious are
// both here:
//
//   * `it-common`'s `Utils` pins the shared JAXB marshaller to `JAXB_ENCODING = "ISO-8859-1"`, so a
//     Latin-1 envelope is not a legacy curiosity — it is what a live supplier reply looks like. A
//     UTF-8-only decode would turn `São Paulo` into something that reads like a *supplier* bug,
//     which is the expensive kind of wrong answer.
//   * `someArray.length` failed, because the read routed through the field lookup and a JDWP array type
//     has no field table.
//
// Every string here is written with unicode escapes rather than literal accented characters, so the
// source stays pure ASCII and cannot be re-encoded by an editor or a checkout into something that
// quietly changes what the probe holds — which is precisely the failure the test is about. Note javac
// expands those escapes BEFORE it lexes, which is also why one cannot be written into a comment: this
// paragraph has to describe them rather than show one, or the file stops compiling.
public class BytesProbe {

    /// The payload, in the form a marshalled envelope actually arrives in: an XML declaration, newlines,
    /// and a non-ASCII character that is the whole question. `ã` is `ã`.
    ///
    /// The newlines matter as much as the accent. A trace record is ONE line, so a decoded envelope has
    /// to escape them or it breaks the record apart — and until something holds a payload with a newline
    /// in it, nothing proves that it does.
    static final String XML =
            "<?xml version=\"1.0\"?>\n<Envelope>\n  <cidade>S\u00e3o Paulo</cidade>\n</Envelope>";

    /// `São Paulo` alone, short enough that a whole render fits in the default 200-char cap.
    static final String CITY = "S\u00e3o Paulo";

    /// The `WSIntegradorLog` shape: one request and one response per supplier round trip, as `byte[]`.
    ///
    /// The two are encoded DIFFERENTLY on purpose. That is not a contrived asymmetry — a request this
    /// stack marshals goes out through the ISO-8859-1 marshaller while a modern supplier answers in
    /// UTF-8, so both readings are in circulation within one round trip and a caller has to be able to
    /// pick per value rather than per call.
    public static class Log {
        byte[] dsRequest;
        byte[] dsResponse;

        Log(byte[] request, byte[] response) {
            dsRequest = request;
            dsResponse = response;
        }
    }

    static byte[] latin1Xml;   // XML as ISO-8859-1: 0xE3 stands alone, which is NOT valid UTF-8
    static byte[] utf8Xml;     // the same XML as UTF-8
    static byte[] latin1City;  // "São Paulo" as ISO-8859-1 — short, so nothing is truncated
    static byte[] utf8City;    // the same as UTF-8

    /// Not text at all: a NUL, a byte no charset makes printable, and DEL. The reason `#raw` exists —
    /// a byte[] really can be a hash or a serialised object, and for those the octets ARE the answer.
    static byte[] blob = {0, 1, -2, 127};

    /// A `char[]`, which carries no charset question — a Java `char` is already a UTF-16 code unit.
    /// Element 2 is a LONE SURROGATE, the TYPE-1 (#48) case: not a character, and it must stay
    /// distinguishable from a `?` the debuggee could really hold.
    static char[] chars = {'o', 'l', (char) 0xD800};

    /// The three array kinds `.length` has to answer for: a primitive byte[] (above), an object array,
    /// and a primitive int[].
    static String[] words = {"alpha", "beta", "gamma"};
    static int[] numbers = {10, 20, 30, 40, 50};

    /// An array OF byte[]s, so an index can be followed by `.length` — and so the outer array stays an
    /// element list while each element reads as text.
    static byte[][] pages;

    static Log log;

    static {
        try {
            latin1Xml = XML.getBytes("ISO-8859-1");
            utf8Xml = XML.getBytes("UTF-8");
            latin1City = CITY.getBytes("ISO-8859-1");
            utf8City = CITY.getBytes("UTF-8");
        } catch (java.io.UnsupportedEncodingException e) {
            throw new ExceptionInInitializerError(e);
        }
        pages = new byte[][] {latin1City, utf8City};
        log = new Log(latin1Xml, utf8Xml);
    }

    /// Somewhere for the marker statement to write, so it cannot be optimised away.
    static int touched;

    /// The same values as parameters and locals, so the frame-local path is exercised alongside the
    /// static-field one.
    static void work(Log entry, byte[] req, byte[] resp, int n) {
        int size = req.length + resp.length;
        System.out.println("work " + n + " " + size + " " + entry.dsRequest.length);
        // The marker sits on a statement of its own, on ONE line: a `// BP<n>` on the last line of a
        // statement spanning several is a line the compiler emitted no code for, so the breakpoint arms
        // against nothing and the test times out saying only "never fired".
        touched = size; // BP1: entry, req, resp, n and size are all in scope here
        touched = size + 1; // BP2: a second location, so two traced stop points need not share one
    }

    public static void main(String[] args) throws Exception {
        for (int n = 0; n < 100000; n++) {
            work(log, latin1City, utf8City, n);
            System.out.println("tick " + n);
            Thread.sleep(150);
        }
    }
}
