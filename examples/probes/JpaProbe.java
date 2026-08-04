// Probe for EVAL-11 (#124) — a named JPA query run against a live EntityManager, reproduced structurally.
//
//   javac -g JpaProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8822 -cp . \
//        jakarta.persistence.JpaProbe
//
// WHAT THIS PROVES AND WHAT IT DOES NOT. The suite cannot depend on hibernate-core, a JPA API jar or a
// database being on the box — `javac_into_memory` runs `javac` with no `-cp` at all — so this reproduces the
// SHAPE the tool drives rather than running real JPA. It proves the mechanism: that discovery finds the bean
// by the fully-qualified interface name, that a named query is looked up by name and an unknown one is
// refused distinguishably, that an all-null optional-parameter query over-matches, that the flush the tool
// suppresses would otherwise have happened, and that rendering the rows invokes NOTHING on them.
//
// It does NOT prove that real Hibernate behaves this way, and nothing that runs without Hibernate could.
// What is real here is the API surface — every name and signature below is taken from the JPA spec:
//
//   jakarta.persistence.EntityManager      createNamedQuery(String) — JPA 2.0+, unchanged in Jakarta
//   jakarta.persistence.Query              setParameter(String, Object), setParameter(int, Object),
//                                          setFlushMode(FlushModeType), setMaxResults(int), getResultList()
//   jakarta.persistence.FlushModeType      AUTO (the spec default) and COMMIT
//   getQueryString()                       NOT JPA — org.hibernate.query.Query, which is why the tool reads
//                                          it best-effort and says so when it is absent
//
// THE PACKAGE IS THE POINT, which is why this is the second probe to declare one (`Probe::launch_in_package`,
// added for EVAL-9). Discovery turns on the fully-qualified name `jakarta.persistence.EntityManager`, so a
// stand-in has to be in that package or the interface check would correctly refuse to recognise it. One
// `.java` declares one package, which puts the probe's own class there too.
//
// The three named queries below are the shapes #124 was filed about. `Reserva.findByCodigoAndStatus` is the
// real bug it describes: both parameters are optional and null-guarded, so both coming in null matches every
// row in the table rather than the one row the caller wanted. `Reserva.findByCodigoPositional` is the same
// predicate bound by position, so the ordered form is exercised rather than assumed. `Reserva.broken` throws
// when RUN rather than when looked up, because "you named a query that does not exist" and "your query blew
// up" are different diagnoses and must not share a message.
package jakarta.persistence;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * The real JPA interface at its real fully-qualified name — `jakarta.persistence.EntityManager`.
 *
 * **Top-level and not nested, which is the whole trick** (the same one `LazyProxyProbe` documents). Nested
 * inside `JpaProbe` it would compile to `Ljakarta/persistence/JpaProbe$EntityManager;` and the interface
 * check would correctly refuse to recognise it. Package-private is fine: a JNI signature carries no
 * accessibility, and one `.java` may hold several top-level types as long as only the one matching the file
 * name is public.
 *
 * Reduced to the one method the tool calls. A real `EntityManager` has about forty, none of which this
 * touches — `persist`, `merge`, `remove`, `flush` and `getTransaction` are deliberately absent so that a
 * tool which reached for any of them would fail to compile against this shape rather than silently work.
 */
interface EntityManager {
    Query createNamedQuery(String name);
}

/** The real `jakarta.persistence.Query`, reduced to the calls the tool makes, with the spec's signatures. */
interface Query {
    Query setParameter(String name, Object value);

    Query setParameter(int position, Object value);

    Query setFlushMode(FlushModeType flushMode);

    Query setMaxResults(int maxResult);

    List getResultList();
}

/**
 * The real `jakarta.persistence.FlushModeType`. **`AUTO` is the spec default**, and that is the fact the
 * whole read-only story turns on: under `AUTO` the provider flushes pending changes before running a query,
 * so merely *asking* a question writes to the database. The tool sets `COMMIT` on the query it created to
 * suppress that, and {@link JpaProbe#flushes} counts what would have happened without it.
 */
enum FlushModeType {
    COMMIT,
    AUTO
}

public class JpaProbe {

    // ----- what a test reads to prove the tool touched nothing it should not -----

    /**
     * How many times `getResultList()` flushed. **Stays 0 only if the tool suppressed the flush**, because
     * every query below starts at the spec default of `AUTO`.
     */
    public static int flushes = 0;

    /**
     * How many times anything invoked a method on a row's lazy association. **Stays 0 only if the row
     * projection invoked nothing**, which is #124's third acceptance criterion. A bounded projection that
     * reached for `toString()` would show up here, and in the `WALKED IN` sentinel below.
     */
    public static int associationTouches = 0;

    /** How many rows the last `getResultList()` actually materialised, for the over-match assertion. */
    public static int lastResultSize = 0;

    // ----- the entity, and the association nothing may walk into -----

    /**
     * A row's lazy association, reproduced by its HAZARD rather than by Hibernate's structure.
     *
     * `LazyProxyProbe` reproduces `org.hibernate.proxy.HibernateProxy` because the detection under test
     * turns on that name. Nothing here does: the criterion is that the projection invokes *nothing* on a
     * nested object, so the stronger proof is an object that RECORDS being touched and returns a value no
     * correct reply could contain. A test asserting only on the absence of a Hibernate marker would pass
     * against a tool that called `toString()` on an ordinary association; this one cannot.
     */
    public static class Itens {
        private final List<String> skus = new ArrayList<>();

        Itens(String sku) {
            skus.add(sku);
        }

        @Override
        public String toString() {
            associationTouches++;
            return "WALKED IN — this value should never reach a caller";
        }

        public List<String> getSkus() {
            associationTouches++;
            return skus;
        }
    }

    /**
     * An entity of primitives only, for measuring what a row's own two reads cost (PERF-1, #100).
     *
     * `Reserva` is the realistic row and the wrong instrument. Rendering one costs far more than the two
     * reads a row needs — its `String` fields are each a `StringReference.Value` round trip and its
     * association is another `ObjectReference.ReferenceType` — so a measurement over `Reserva` is dominated
     * by reads PERF-1 has not converted, and moved by only a fifth when the two it did convert were waved.
     * Measured: 79.19ms of wire time per row before, 63.18ms after, at an 8ms round trip.
     *
     * With nothing but a `long` and a `double`, a row costs exactly `ReferenceType` + `GetValues` and
     * nothing else, so the difference between reading those one at a time and reading them as independent
     * reads is the whole of the per-row cost rather than a fifth of it.
     */
    public static class Bare {
        long id;
        double valor;

        Bare(long id, double valor) {
            this.id = id;
            this.valor = valor;
        }
    }

    /** The entity. Plain fields, because the projection reads fields and calls no getter. */
    public static class Reserva {
        Long id;
        String codigo;
        String status;
        double valor;
        Itens itens;

        Reserva(Long id, String codigo, String status, double valor) {
            this.id = id;
            this.codigo = codigo;
            this.status = status;
            this.valor = valor;
            this.itens = new Itens("SKU-" + id);
        }

        /**
         * Present and never called, on the same principle as the sentinel above: a projection that decided
         * to be helpful by invoking getters would be caught by `associationTouches` going up.
         */
        public String getCodigo() {
            associationTouches++;
            return codigo;
        }
    }

    // ----- the query engine: enough of one to make the over-match real -----

    /**
     * One `@NamedQuery`, as a filter over the table plus the parameters bound so far.
     *
     * The raw `List` return is deliberate and is what `Reserva.mixedTypes` needs: a real provider can hand
     * back rows of more than one type, and the tool has to read each row's fields off the row's OWN type.
     */
    interface NamedQuery {
        List run(List<Reserva> table, Map<Object, Object> params);
    }

    /**
     * The named-query registry. A real provider builds this from `@NamedQuery` annotations and `orm.xml` at
     * bootstrap, and — the fact that shapes the tool's error message — **exposes no way to list it**. There
     * is no `getNamedQueries()` on `EntityManager`, which is why an unknown name can be reported clearly but
     * the known ones cannot be suggested.
     */
    static final Map<String, NamedQuery> QUERIES = new HashMap<>();

    /** Each query's JPQL, so `getQueryString()` answers about the query it was actually asked about. */
    static final Map<String, String> TEXTS = new HashMap<>();

    static {
        // #124's actual bug. Both parameters are optional and null-guarded, exactly as the JPQL
        //     WHERE (:codigo IS NULL OR r.codigo = :codigo)
        //       AND (:status IS NULL OR r.status = :status)
        // reads — so both null matches the whole table instead of the one row the caller meant.
        TEXTS.put("Reserva.findByCodigoAndStatus",
                "select r from Reserva r where (:codigo is null or r.codigo = :codigo) "
                        + "and (:status is null or r.status = :status)");
        QUERIES.put("Reserva.findByCodigoAndStatus", (table, params) -> {
            Object codigo = params.get("codigo");
            Object status = params.get("status");
            List<Reserva> hits = new ArrayList<>();
            for (Reserva r : table) {
                boolean codigoOk = codigo == null || codigo.equals(r.codigo);
                boolean statusOk = status == null || status.equals(r.status);
                if (codigoOk && statusOk) {
                    hits.add(r);
                }
            }
            return hits;
        });

        // The same query bound POSITIONALLY (1-based, as JPQL's `?1` is), so the ordered parameter form is
        // exercised against a real overload rather than assumed to work.
        TEXTS.put("Reserva.findByCodigoPositional",
                "select r from Reserva r where (?1 is null or r.codigo = ?1)");
        QUERIES.put("Reserva.findByCodigoPositional", (table, params) -> {
            Object codigo = params.get(1);
            List<Reserva> hits = new ArrayList<>();
            for (Reserva r : table) {
                if (codigo == null || codigo.equals(r.codigo)) {
                    hits.add(r);
                }
            }
            return hits;
        });

        // A result set of MORE THAN ONE TYPE, which is what makes the per-row type read a dependency rather
        // than a convenience (PERF-1, #100). The tool reads every row's type in one wave and every row's
        // fields in a second, and the second cannot be folded into the first: a `Reserva`'s field ids mean
        // nothing to an `Itens`, so a values read issued before its own type read came back would ask the
        // JVM for the wrong fields. This query makes that visible in a reply instead of arguable in a
        // review — the rows alternate, and `Itens` has no field `codigo` for the wrong wave to find.
        TEXTS.put("Reserva.mixedTypes", "select r, r.itens from Reserva r");
        QUERIES.put("Reserva.mixedTypes", (table, params) -> {
            List rows = new ArrayList();
            for (Reserva r : table) {
                rows.add(r);
                rows.add(r.itens);
            }
            return rows;
        });

        // Rows of primitives only, so a per-row measurement is not swamped by what rendering a `String`
        // or an association costs. See `Bare`.
        TEXTS.put("Bare.all", "select b from Bare b");
        QUERIES.put("Bare.all", (table, params) -> {
            List rows = new ArrayList();
            for (Reserva r : table) {
                rows.add(new Bare(r.id, r.valor));
            }
            return rows;
        });

        // A query that throws when RUN rather than when looked up. The two failures need different
        // messages: one is "you named a query that does not exist", the other is "your query blew up".
        TEXTS.put("Reserva.broken", "select r from Reserva r where r.explodes = true");
        QUERIES.put("Reserva.broken", (table, params) -> {
            throw new IllegalStateException("the named query itself threw");
        });
    }

    /** The `Query` a provider hands back from `createNamedQuery`. */
    static class ProbeQuery implements Query {
        private final String name;
        private final NamedQuery body;
        private final List<Reserva> table;
        private final Map<Object, Object> params = new HashMap<>();

        /** Starts at the spec default, so suppressing it is a real change and not a no-op. */
        FlushModeType flushMode = FlushModeType.AUTO;
        int maxResults = -1;

        ProbeQuery(String name, NamedQuery body, List<Reserva> table) {
            this.name = name;
            this.body = body;
            this.table = table;
        }

        @Override
        public Query setParameter(String name, Object value) {
            params.put(name, value);
            return this;
        }

        @Override
        public Query setParameter(int position, Object value) {
            params.put(position, value);
            return this;
        }

        @Override
        public Query setFlushMode(FlushModeType flushMode) {
            this.flushMode = flushMode;
            return this;
        }

        @Override
        public Query setMaxResults(int maxResult) {
            this.maxResults = maxResult;
            return this;
        }

        /**
         * NOT part of JPA — this is `org.hibernate.query.Query.getQueryString()`, present because a real
         * Hibernate `Query` has it and the tool reads it best-effort. What it returns is the JPQL, never the
         * SQL, which is the distinction the reply has to keep.
         *
         * Keyed by name rather than hardcoded, and the first cut was hardcoded: every query then reported
         * the null-guarded one's text, so a positional query's reply confidently showed JPQL naming
         * `:codigo`. A stand-in that returns the same answer whatever it is asked cannot show that the tool
         * read the RIGHT query back.
         */
        public String getQueryString() {
            return TEXTS.getOrDefault(name, "<no text recorded for " + name + ">");
        }

        @Override
        public List getResultList() {
            // The write a read performs. Under the default AUTO this is where a real provider pushes pending
            // changes to the database before answering — so a tool that left the flush mode alone would have
            // written to a shared database by asking a question.
            if (flushMode == FlushModeType.AUTO) {
                flushes++;
            }
            List hits = body.run(table, params);
            if (maxResults >= 0 && hits.size() > maxResults) {
                hits = new ArrayList(hits.subList(0, maxResults));
            }
            lastResultSize = hits.size();
            return hits;
        }
    }

    /** The provider's `EntityManager`. */
    static class ProbeEntityManager implements EntityManager {
        private final List<Reserva> table;

        ProbeEntityManager(List<Reserva> table) {
            this.table = table;
        }

        @Override
        public Query createNamedQuery(String name) {
            NamedQuery body = QUERIES.get(name);
            if (body == null) {
                // The spec's own behaviour, and the reason #124's second criterion exists: an unknown name
                // is an IllegalArgumentException indistinguishable from any other bad argument unless the
                // tool looks at what it asked for.
                throw new IllegalArgumentException("No query defined for that name [" + name + "]");
            }
            return new ProbeQuery(name, body, table);
        }
    }

    // ----- reachability: one route per discovery path -----

    /**
     * Holds the `EntityManager` where NO frame can name it — the container's job in a real deployment, and
     * the case `debug.list_instances` was built for (#84). A static field of a class the suspended frame has
     * nothing to do with is invisible to a scan of `this` and the locals, so this is what forces the heap
     * route.
     */
    static class ProbeContainer {
        static EntityManager em;
    }

    /** How many rows the table holds. Every one matches when both optional parameters come in null. */
    public static final int TABLE_ROWS = 1000;

    static List<Reserva> buildTable() {
        List<Reserva> table = new ArrayList<>();
        for (int i = 1; i <= TABLE_ROWS; i++) {
            // One row is findable by code; the rest share two statuses, so a single-parameter bind is a
            // partial match rather than either extreme.
            String codigo = i == 7 ? "R-7" : "R-" + i;
            String status = i % 2 == 0 ? "CONFIRMADA" : "PENDENTE";
            table.add(new Reserva((long) i, codigo, status, i * 10.5));
        }
        return table;
    }

    /**
     * A frame with NO `EntityManager` anywhere in it — not as a parameter, not as a local, and `this` is
     * absent because it is static. Suspending here leaves the heap route as the only way to find the bean.
     */
    static void workWithoutEm(int i) {
        String note = "no bean in this frame";
        System.out.println("tick " + i + " " + note); // BP_HEAP
    }

    /** A frame that holds the bean as a parameter, so the free route can find it. */
    static void workWithEm(EntityManager em, int i) {
        System.out.println("frame tick " + i); // BP_FRAME
    }

    public static void main(String[] args) throws Exception {
        List<Reserva> table = buildTable();
        ProbeContainer.em = new ProbeEntityManager(table);

        // Printed once so a test can prove the shapes are what it thinks before asserting anything.
        System.out.println("em is " + ProbeContainer.em.getClass().getName());
        System.out.println("em implements the interface: " + (ProbeContainer.em instanceof EntityManager));
        System.out.println("table rows: " + table.size());

        for (int i = 0; i < 100000; i++) {
            workWithoutEm(i);
            workWithEm(ProbeContainer.em, i);
            Thread.sleep(150);
        }
    }
}
