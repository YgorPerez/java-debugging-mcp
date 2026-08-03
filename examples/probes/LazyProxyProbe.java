// Probe for EVAL-9 (#86) — an UNINITIALISED Hibernate entity proxy, reproduced structurally.
//
//   javac -g LazyProxyProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8822 -cp . \
//        org.hibernate.proxy.LazyProxyProbe
//
// WHAT THIS PROVES AND WHAT IT DOES NOT. The suite cannot depend on hibernate-core being on the box, so
// this reproduces the SHAPE the detection reads rather than running real Hibernate. It proves the logic:
// that the marker interface is what decides, that the `initialized` flag is found through a private field
// several classes up, that a method call and an inherited field read are both refused, that the proxy's own
// declared fields are still readable, and that nothing is invoked to find any of it out.
//
// It does NOT prove the NAMES, and nothing that runs on a machine without Hibernate could. Those were
// measured separately and are recorded on #86: `javap` against hibernate-core 3.5.6-Final, 4.3.1.Final and
// 5.4.25.Final for the field names, and a real detached proxy built with `ByteBuddyProxyFactory` and a null
// session for the rest — through this debugger, which read
// `proxy.$$_hibernate_interceptor.initialized = false` by pure field reads and then watched
// `force_initialize:true` throw `LazyInitializationException`.
//
// THE PACKAGE IS THE POINT, which is why this is the one probe that declares one: the detection turns on the
// fully-qualified name `org.hibernate.proxy.HibernateProxy`, so a stand-in has to be in that package or the
// interface check would correctly refuse to recognise it. `Probe::launch_in_package` exists for this.
//
// Every name below is the real one, and none of them is a guess:
//
//   HibernateProxy                 org.hibernate.proxy.HibernateProxy — the marker, unchanged since 3.x
//   $$_hibernate_interceptor       ProxyConfiguration.INTERCEPTOR_FIELD_NAME, Hibernate 5.3+ (Byte Buddy)
//   handler                        javassist.util.proxy.ProxyFactory.HANDLER, Hibernate 3.x–5.2
//   initialized                    AbstractLazyInitializer.initialized, private, in ALL THREE generations
//   $HibernateProxy$               ByteBuddyProxyHelper.PROXY_NAMING_SUFFIX, seen live as
//                                  RealHibernateProbe$Order$HibernateProxy$6OHhgouN
package org.hibernate.proxy;

/**
 * The real marker interface at its real fully-qualified name — `org.hibernate.proxy.HibernateProxy`.
 *
 * **Top-level and not nested, which is the whole trick.** Nested inside `LazyProxyProbe` it would compile to
 * `Lorg/hibernate/proxy/LazyProxyProbe$HibernateProxy;` and the interface check would correctly refuse to
 * recognise it — the first version of this probe made exactly that mistake. Package-private is fine: a JNI
 * signature carries no accessibility, and one `.java` may hold several top-level types as long as only the
 * one matching the file name is public.
 */
interface HibernateProxy {}

public class LazyProxyProbe {

    /**
     * Hibernate's own `AbstractLazyInitializer`, reduced to the one field that matters. `initialized` is
     * `private` and on a SUPERCLASS on purpose — that is where it really lives, three classes below
     * `ByteBuddyInterceptor`, and a detection that only looked at declared fields would miss it.
     */
    static class AbstractLazyInitializer {
        private boolean initialized;
        private final Object id;

        AbstractLazyInitializer(Object id, boolean initialized) {
            this.id = id;
            this.initialized = initialized;
        }
    }

    /** The concrete interceptor, so `initialized` is genuinely inherited rather than declared. */
    static class ByteBuddyInterceptor extends AbstractLazyInitializer {
        ByteBuddyInterceptor(Object id, boolean initialized) {
            super(id, initialized);
        }
    }

    /** The entity. Its fields are what a proxy inherits and never populates. */
    public static class Order {
        Long id;
        String ref;

        Order(Long id, String ref) {
            this.id = id;
            this.ref = ref;
        }

        public String getRef() {
            return ref;
        }
    }

    /**
     * The proxy. `$` is a legal Java identifier character, so this compiles to a class literally named
     * `org.hibernate.proxy.LazyProxyProbe$Order$HibernateProxy$Stub` — carrying the `$HibernateProxy$`
     * infix Byte Buddy's naming strategy produces.
     *
     * **It extends `Order` and never populates its inherited fields**, which is the measured hazard: on a
     * real proxy `.id` reads null while the proxy's identity is 42, so a field read is a wrong answer with
     * no error at all. `getRef()` returning a lie rather than throwing makes the same point louder — a test
     * that got this value back would have been told something false by a tool that reported success.
     */
    public static class Order$HibernateProxy$Stub extends Order implements HibernateProxy {
        /** Hibernate 5.3+ spelling. Declared on the proxy, so reading it is the proxy's OWN state. */
        final Object $$_hibernate_interceptor;

        Order$HibernateProxy$Stub(boolean initialized) {
            super(null, null); // exactly what a proxy does: inherited fields stay unpopulated
            this.$$_hibernate_interceptor = new ByteBuddyInterceptor(42L, initialized);
        }

        @Override
        public String getRef() {
            return "WALKED IN — this value should never reach a caller";
        }
    }

    /** The Hibernate 3.x/4.x spelling, so the fallback field name is exercised too. */
    public static class Order$HibernateProxy$Javassist extends Order implements HibernateProxy {
        final Object handler;

        Order$HibernateProxy$Javassist(boolean initialized) {
            super(null, null);
            this.handler = new ByteBuddyInterceptor(7L, initialized);
        }

        @Override
        public String getRef() {
            return "WALKED IN — this value should never reach a caller";
        }
    }

    /** Named like a proxy and implementing NOTHING, so the interface — not the name — has to decide. */
    public static class Order$HibernateProxy$NotReally extends Order {
        Order$HibernateProxy$NotReally() {
            super(3L, "an ordinary object that happens to be named this way");
        }
    }

    // The statics a test reads. Held in statics so every one is reachable with nothing suspended.
    public static Object unloaded = new Order$HibernateProxy$Stub(false);
    public static Object loaded = new Order$HibernateProxy$Stub(true);
    public static Object unloadedJavassist = new Order$HibernateProxy$Javassist(false);
    public static Object notAProxy = new Order$HibernateProxy$NotReally();
    public static Object plainEntity = new Order(9L, "plain");

    public static void main(String[] args) throws Exception {
        // Printed once so a test can prove the shapes are what it thinks before asserting anything.
        System.out.println("unloaded is " + unloaded.getClass().getName());
        System.out.println("proxy implements marker: " + (unloaded instanceof HibernateProxy));
        for (int i = 0; i < 100000; i++) {
            System.out.println("tick " + i); // BP1 — a suspending stop here is what
            // gives the invoking assertions a thread suspended BY AN EVENT, which is the only
            // kind JDWP will run a method on.
            Thread.sleep(150);
        }
    }
}
