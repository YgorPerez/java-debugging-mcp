// Probe for TRACE-9 (raising the per-value capture length), driven by mcp_integration.rs.
//
//   javac -g PayloadProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8817 -cp . PayloadProbe
//
// The point of this probe is a payload that is REALISTICALLY long — a gateway response body, the shape
// the shared 8180 is full of — held in a local at the traced line and reachable through a getter, so one
// hit exercises both capped slots at once: the in-scope local (100 chars by default) and the trace_expr
// result (200).
//
// Two things about the payload are load-bearing:
//
//   - it is EXACTLY PAYLOAD_LENGTH characters, so a test can assert the `… (N chars total)` suffix the
//     debugger appends rather than asserting "some truncation happened";
//   - the marker TAILMARK appears ONLY in the last field. Anything a default capture keeps — the opening
//     brace, MerchantOrderId, the customer block — is present in both the truncated and the raised
//     rendering, so asserting on it would pass even if trace_max_length were ignored entirely. The tail
//     marker is the one string that can only be there because the cap was actually raised.
//
// `handle` deliberately assigns rather than prints: the traced line must not be optimised away, and the
// snapshot's payload should be the local, not a side effect.
//
// The tick line is load-bearing, as in CallerProbe: a traced stop point must let the probe keep printing,
// which is what actually proves no thread was left suspended. A suspending one stops the ticks dead, and
// the debugger reports success either way.
public class PayloadProbe {

    // Sized to the issue's own example — roughly what a payment gateway or a SOAP envelope returns, and
    // several times either default cap, so a default capture cannot reach the end of it by accident.
    static final int PAYLOAD_LENGTH = 2048;

    // Present ONLY at the very end of the payload. See the header: this is the whole assertion.
    static final String TAIL_MARKER = "TAILMARK";

    /** One gateway response, holding its own body — the getter is what a trace_expr calls. */
    static class GatewayResponse {
        private final String body;

        GatewayResponse(String body) {
            this.body = body;
        }

        String getBody() {
            return body;
        }
    }

    // A JSON body of exactly PAYLOAD_LENGTH characters, ending in the tail marker. Built rather than
    // written out so the length is a fact rather than a comment that drifts.
    static String buildBody() {
        StringBuilder sb = new StringBuilder();
        sb.append("{\"MerchantOrderId\":\"2014111703\",\"Customer\":{\"Name\":\"Comprador credito completo\",");
        sb.append("\"Identity\":\"11225468795\",\"IdentityType\":\"CPF\",\"Email\":\"compradorteste@teste.com\",");
        sb.append("\"Address\":{\"Street\":\"Rua Teste\",\"Number\":\"123\",\"Complement\":\"AP 123\",");
        sb.append("\"ZipCode\":\"12345987\",\"City\":\"Sao Paulo\",\"State\":\"SP\",\"Country\":\"BRA\"}},");
        sb.append("\"Items\":[");
        int i = 0;
        // The tail is a fixed-width closing block; grow the items array until the whole body lands exactly
        // one closing block short of PAYLOAD_LENGTH.
        String tail = "],\"Payment\":{\"Amount\":15700,\"Installments\":1,\"Status\":2,"
                + "\"ReturnCode\":\"" + TAIL_MARKER + "\"}}";
        while (sb.length() + tail.length() < PAYLOAD_LENGTH) {
            String item = "{\"Sku\":\"UH-" + (1000 + i) + "\",\"Qty\":1,\"Name\":\"Diaria standard\"},";
            if (sb.length() + item.length() + tail.length() > PAYLOAD_LENGTH) {
                break;
            }
            sb.append(item);
            i++;
        }
        // Pad inside the last item's name so the total is exact, then close.
        int pad = PAYLOAD_LENGTH - sb.length() - tail.length();
        StringBuilder padding = new StringBuilder();
        for (int j = 0; j < pad; j++) {
            padding.append('x');
        }
        sb.append(padding);
        sb.append(tail);
        return sb.toString();
    }

    static final String BODY = buildBody();

    static int handled = 0;
    static int lastLength = -1;

    // The traced location. `body` is the in-scope local the capture truncates at 100 by default;
    // `response.getBody()` is what a trace_expr calls, truncated at 200 by default.
    static void handle(GatewayResponse response) {
        String body = response.getBody();
        handled++;
        lastLength = body.length(); // BP1
    }

    public static void main(String[] args) throws Exception {
        GatewayResponse response = new GatewayResponse(BODY);
        // Announced once so a test can prove the payload really is the length it asserts against, from
        // the probe's OWN stdout rather than from the debugger's reply.
        System.out.println("payload length=" + BODY.length() + " tail=" + TAIL_MARKER);
        for (int i = 0; i < 100000; i++) {
            handle(response);
            System.out.println("tick " + i + " handled=" + handled + " last=" + lastLength);
            Thread.sleep(150);
        }
    }
}
