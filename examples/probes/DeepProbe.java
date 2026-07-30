// Probe for OBJ-1 (recursive object expansion), driven by mcp_integration.rs.
//
//   javac -g DeepProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8807 -cp . DeepProbe
//
// Deliberately contains every shape the deep renderer has to survive:
//   - nested plain objects (order → customer → address), to check depth bounding
//   - a self-reference (customer.self) and a parent↔child cycle (order.customer.lastOrder == order),
//     which is what proves cycle detection rather than a stack overflow
//   - a List, a Map, a Set and an Optional, for element-level rendering
//   - a List longer than the default child limit, to check "… +N more"
//   - an empty list and an empty Optional, the usual off-by-one traps
//   - an int[] and a String[], since arrays take a different path from collections
//   - an inherited field (Order extends Record), which must show up alongside declared ones
//   - a List<Line> with a mix of paid/unpaid and varying totals, for OBJ-2 slice/filter subscripts
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;

public class DeepProbe {

    static class Record {
        // Inherited by Order: field collection must walk superclasses, not just the concrete type.
        int recordId = 7;
    }

    // Element type for the OBJ-2 subscript tests: a field, a getter and a boolean, so predicates can
    // be written against each.
    static class Line {
        String sku;
        int qty;
        boolean paid;

        Line(String sku, int qty, boolean paid) {
            this.sku = sku;
            this.qty = qty;
            this.paid = paid;
        }

        int getQty() {
            return qty;
        }

        @Override public String toString() {
            return "Line(" + sku + "," + qty + "," + paid + ")";
        }
    }

    static class Address {
        String city = "Lisbon";
        int zip = 1000;
    }

    static class Customer {
        String name = "Ana";
        Address address = new Address();
        // Self-reference: the shortest possible cycle.
        Customer self;
        // Back-reference to the owning order: a two-hop cycle.
        Order lastOrder;

        Customer() {
            this.self = this;
        }
    }

    static class Order extends Record {
        int id = 42;
        String status = "OPEN";
        double total = 19.5;
        boolean paid = false;
        Customer customer = new Customer();
        List<String> tags = new ArrayList<>();
        List<Integer> many = new ArrayList<>();
        List<String> empty = new ArrayList<>();
        Map<String, Integer> counts = new LinkedHashMap<>();
        Set<String> labels = new LinkedHashSet<>();
        Optional<String> note = Optional.of("gift");
        Optional<String> missing = Optional.empty();
        int[] numbers = {1, 2, 3};
        String[] words = {"alpha", "beta"};
        List<Line> lines = new ArrayList<>();
        // OBJ-4: a Map whose VALUES have fields worth filtering on, so `byId[?qty > 3]` has real work
        // and the surviving entries have keys worth keeping.
        Map<String, Line> byId = new LinkedHashMap<>();
        // EVAL-10 (#92): a Map that is NOT one of the layouts read structurally, wrapping one that is.
        // A subscript on it therefore still has to invoke get() in the debuggee, which is what makes it
        // the read-only refusal case now that `counts["a"]` is a plain field walk.
        Map<String, Integer> wrappedCounts = Collections.synchronizedMap(counts);
        int threshold = 3;

        Order() {
            customer.lastOrder = this;
            tags.add("urgent");
            tags.add("fragile");
            // 20 entries, above the default child limit of 16, so "… +N more" must appear.
            for (int i = 0; i < 20; i++) {
                many.add(i);
            }
            counts.put("a", 1);
            counts.put("b", 2);
            labels.add("x");
            labels.add("y");
            // 2 paid, 3 unpaid; qty spans the threshold both ways, so predicates have real work.
            lines.add(new Line("aa", 1, true));
            lines.add(new Line("bb", 5, false));
            lines.add(new Line("cc", 2, true));
            lines.add(new Line("dd", 9, false));
            lines.add(new Line("ee", 4, false));
            for (Line l : lines) {
                byId.put(l.sku, l);
            }
        }
    }

    // OBJ-3: a SECOND, deliberately heavy local in the same frame as `order`, so one get_stack call
    // has more than one expandable local to spend its node budget on. The same order 20 times over —
    // siblings, not a cycle, so each renders in full (path-based detection only stops a value
    // reachable from itself), which is ~500 nodes per handful. With the old per-local budget this one
    // frame could spend several times the documented cap; with one shared budget it must stop.
    static List<Order> batchOf(Order order) {
        List<Order> batch = new ArrayList<>();
        for (int i = 0; i < 20; i++) {
            batch.add(order);
        }
        return batch;
    }

    static void inspect(Order order, int n) {
        int local = n;
        List<Order> batch = batchOf(order);
        System.out.println("inspect " + local + " " + order.id + " " + batch.size()); // BP1
    }

    public static void main(String[] args) throws Exception {
        Order order = new Order();
        for (int i = 0; i < 100000; i++) {
            inspect(order, i);
            Thread.sleep(150);
        }
    }
}
