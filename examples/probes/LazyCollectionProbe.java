// Probe for EVAL-9 (#86) — an UNINITIALISED Hibernate persistent COLLECTION, reproduced structurally.
//
//   javac -g LazyCollectionProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8823 -cp . \
//        org.hibernate.collection.spi.LazyCollectionProbe
//
// The other half of EVAL-9, and the commoner one: the measured 1897 `FetchType.LAZY` associations are mostly
// `@OneToMany` collections, and a collection behaves differently from an entity proxy in a way the report has
// to reflect.
//
//   - The getter that returns it triggers NOTHING. `reserva.getReservaHotelList()` hands back a
//     `PersistentBag` without a single SELECT, so holding one is harmless and a debugger that refused the
//     getter would be refusing something safe.
//   - It is the NEXT link that loads it — `.size()`, iteration, a subscript. So the sentence is "the contents
//     have not been fetched and the next link is what would fetch them", not "this link is unreadable".
//   - A FIELD read is safe here, unlike on a proxy. The collection is nobody's stand-in; its fields ARE its
//     state, `initialized` included. Getting that wrong was caught by running the first implementation
//     against a real `PersistentBag`, which refused to answer `.initialized` — a read that triggers nothing
//     at all.
//
// WHAT THIS PROVES: the same as `LazyProxyProbe` — the logic, not the names. The names came from `javap`
// against hibernate-core 3.5.6-Final, 4.3.1.Final and 5.4.25.Final (`AbstractPersistentCollection.initialized`
// is `private boolean` in all three) and from a real uninitialised `PersistentBag` built with a null session,
// against which this debugger reported the unfetched collection, refused `.size()`, and answered
// `.initialized = false`. #86 records both.
//
// THE PACKAGE IS THE POINT. `org.hibernate.collection.spi.PersistentCollection` is the interface that decides,
// and `org.hibernate.collection.` is the prefix that makes a class a candidate cheaply — so the stand-in has
// to live there. Both the interface and the stubs go in this one file because one `.java` declares one
// package, and `spi` satisfies both requirements at once: the interface's real home, and inside the
// `org.hibernate.collection` prefix.
package org.hibernate.collection.spi;

/**
 * The real marker interface at its real name. Top-level rather than nested, for the reason
 * `LazyProxyProbe` spells out: nested, its signature would carry the outer class and the check would
 * correctly not recognise it.
 */
interface PersistentCollection {}

/**
 * Hibernate's `AbstractPersistentCollection`, reduced to the one field. `initialized` is `private` and on the
 * SUPERCLASS because that is where it really is — `PersistentBag` inherits it — so a detection that only read
 * declared fields would miss every real collection.
 */
abstract class AbstractPersistentCollection implements PersistentCollection {
    private boolean initialized;

    AbstractPersistentCollection(boolean initialized) {
        this.initialized = initialized;
    }
}

public class LazyCollectionProbe {

    /** The concrete collection, so the flag is genuinely inherited. */
    public static class PersistentBag extends AbstractPersistentCollection {
        /** Declared on the bag itself, so a test can tell an own field from an inherited one. */
        final String role;

        PersistentBag(boolean initialized, String role) {
            super(initialized);
            this.role = role;
        }

        /** The load. Reaching this at all is the bug — it stands for the deferred SELECT. */
        public int size() {
            return -1; // a value no real collection can return, so it is unmistakable in a failure message
        }
    }

    /**
     * In `org.hibernate.collection.` and implementing NOTHING, so the interface — not the package — is what
     * decides. Without this the check could pass by recognising a prefix, which would report an unfetched
     * collection for any class anyone put in that package.
     */
    public static class NotACollection {
        public int size() {
            return 3;
        }
    }

    public static Object unfetched = new PersistentBag(false, "Reserva.reservaHotelList");
    public static Object fetched = new PersistentBag(true, "Reserva.reservaQuartoList");
    public static Object notACollection = new NotACollection();

    public static void main(String[] args) throws Exception {
        System.out.println("unfetched is " + unfetched.getClass().getName());
        System.out.println("implements marker: " + (unfetched instanceof PersistentCollection));
        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i); // BP1 — a suspending stop here is what
            // gives the invoking assertions a thread suspended BY AN EVENT, which is the only
            // kind JDWP will run a method on.
            Thread.sleep(150);
        }
    }
}
