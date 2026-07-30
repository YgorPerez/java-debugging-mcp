// Probe for EVAL-10 (#92): the four JDK collection layouts that are read by WALKING THEIR INTERNALS
// instead of invoking `get()` / `entrySet()` / `toArray()` in the debuggee.
//
// Every collection here is held in a STATIC field, because that is the shape the feature exists for: a
// static field is readable with no suspended thread, and until EVAL-10 the step from "here is the map"
// to "here is the entry I care about" was not.
//
// Each recognised collection is paired with a wrapper holding THE SAME OBJECT — Collections
// .synchronizedMap / .unmodifiableList, plus a HashMap SUBCLASS. None of those is a recognised layout,
// so reading one goes through the invoking path. That is what lets a test assert the two paths agree
// on identical data rather than against hardcoded expectations, so they cannot drift apart.
//
//   javac -g CollectionProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8794 -cp . CollectionProbe
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

public class CollectionProbe {

    public static class Item {
        String sku;
        int qty;
        Item(String s, int q) { sku = s; qty = q; }
        @Override public String toString() { return "Item(" + sku + "," + qty + ")"; }
    }

    /**
     * A HashMap SUBCLASS. Its internals are a HashMap's, but recognition keys on the runtime type's
     * EXACT signature — because the next subclass along may not keep its entries in `table` at all —
     * so this must fall back to invoking rather than be walked.
     */
    public static class MyMap extends HashMap<String, Item> { }

    // --- the four recognised layouts ---
    static final HashMap<String, Item> HASH = new HashMap<>();
    static final LinkedHashMap<String, Item> LINKED = new LinkedHashMap<>();
    static final ConcurrentHashMap<String, Item> CONCURRENT = new ConcurrentHashMap<>();
    static final HashMap<Integer, Item> BY_ID = new HashMap<>();
    // Capacity 64 holding 5 elements: `elementData` is 59 slots longer than the list, and every one of
    // those slots is null. A read that returned the backing array would leak them as elements.
    static final ArrayList<Item> LIST = new ArrayList<>(64);

    // 16 keys whose hashCodes are all equal ("Aa" and "BB" hash alike, and so does any concatenation
    // of them), in a table sized 64 from the very first put — so the bin TREEIFIES at eight nodes
    // instead of resizing, and the walk meets HashMap$TreeNode rather than HashMap$Node.
    static final String[] COLLIDING = colliding();
    static final HashMap<String, Item> TREEIFIED = new HashMap<>(64);

    // --- the same contents behind implementations that are NOT recognised layouts ---
    static final Map<String, Item> HASH_WRAPPED = Collections.synchronizedMap(HASH);
    static final Map<String, Item> LINKED_WRAPPED = Collections.synchronizedMap(LINKED);
    static final Map<String, Item> CONCURRENT_WRAPPED = Collections.synchronizedMap(CONCURRENT);
    static final Map<Integer, Item> BY_ID_WRAPPED = Collections.synchronizedMap(BY_ID);
    static final Map<String, Item> TREEIFIED_WRAPPED = Collections.synchronizedMap(TREEIFIED);
    static final List<Item> LIST_WRAPPED = Collections.unmodifiableList(LIST);
    static final MyMap SUBCLASS = new MyMap();

    static String[] colliding() {
        String[] two = {"Aa", "BB"};
        String[] out = new String[16];
        int i = 0;
        for (String a : two) {
            for (String b : two) {
                for (String c : two) {
                    for (String d : two) {
                        out[i++] = a + b + c + d;
                    }
                }
            }
        }
        return out;
    }

    static {
        String[] keys = {"a", "b", "c", "d", "e"};
        String[] skus = {"aa", "bb", "cc", "dd", "ee"};
        int[] qtys = {1, 5, 2, 9, 4};
        Item[] items = new Item[keys.length];
        for (int i = 0; i < keys.length; i++) {
            items[i] = new Item(skus[i], qtys[i]);
        }
        for (int i = 0; i < keys.length; i++) {
            HASH.put(keys[i], items[i]);
            CONCURRENT.put(keys[i], items[i]);
            BY_ID.put(i, items[i]);
            LIST.add(items[i]);
            SUBCLASS.put(keys[i], items[i]);
        }
        // REVERSED on purpose. A LinkedHashMap's iteration order is insertion order, not table order,
        // and with these keys the two coincide unless it is built backwards — so this is what makes a
        // walk of the wrong half (`table[]` instead of `head`/`after`) visible from outside.
        for (int i = keys.length - 1; i >= 0; i--) {
            LINKED.put(keys[i], items[i]);
        }
        for (int i = 0; i < COLLIDING.length; i++) {
            TREEIFIED.put(COLLIDING[i], new Item("t" + i, i));
        }
    }

    // The BP<n> markers are how the tests find their breakpoint lines — they grep this file rather than
    // hardcoding numbers, so editing above here is safe. Keep one marker per line.
    static int inspect(int n) {
        int total = LIST.size() + HASH.size(); // BP1: every collection above is populated
        return total + n;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("CollectionProbe ready: " + HASH.size() + " entries, "
                + COLLIDING.length + " colliding keys");
        for (int i = 0; i < 100000; i++) {
            System.out.println("inspect " + inspect(i));
            Thread.sleep(150);
        }
    }
}
