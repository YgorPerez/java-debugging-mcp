// Probe for FILT-6 (#83) — a `condition` on the three stop-point kinds that never had one.
//
//   javac -g CondKindsProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8825 -cp . CondKindsProbe
//
// `AppException` reproduces the shape that makes this issue expensive rather than merely missing.
// `InfoTravelException` in the target is simultaneously the error type and the validation-control-flow type
// — 812 `ExceptionEnum` values, 247 of them validation, thrown as ordinary flow — so an unfiltered exception
// trace burns its 200-hit budget on validation noise before a real fault lands. And the discriminator cannot
// be the message: `InfoTravelException(ExceptionEnum)` calls no `super(...)` and never sets its message
// field, so `getMessage()` is `null` for 1104 of 3166 constructions.
//
// So `AppException` **deliberately calls no `super(message)`** and its `getMessage()` is null. The only
// usable discriminator is the `cdException` field, which is exactly what a condition on the exception
// INSTANCE can read and what `!` can invert. A probe whose exceptions carried messages would let a test pass
// against an implementation that only ever reads `getMessage()`.
//
// THREE HITS IN EVERY FOUR ARE NOISE, which is the ratio that matters: a condition that matched everything
// and a condition that was never evaluated both look like success on a probe where every hit matches.
//
// The prints bracket the interesting region — `offer` before, `done` after — so a SUSPENDING conditional
// stop leaves stdout ending in an `offer` line with no `done` after it, naming the iteration it stopped on.
// A condition that never matched leaves both lines for every iteration; one that matched everything stops on
// the first. Neither can pass. `tick` is on its own line so a traced stop point can be asserted against the
// probe still ticking.
public class CondKindsProbe {

    /** One iteration in every this-many is the interesting one. */
    static final int CYCLE = 4;
    static final int ODD = 2;

    /** The interesting discriminator value, and the noise value. */
    static final int REAL_FAULT = 999;
    static final int VALIDATION = 1;

    /**
     * The target's exception shape: a code field, and NO message. `getMessage()` returns null on purpose —
     * see the header.
     */
    static class AppException extends RuntimeException {
        private static final long serialVersionUID = 1L;
        final int cdException;

        AppException(int cdException) {
            // No super(message) — this is the whole point of the probe.
            this.cdException = cdException;
        }
    }

    /** Watched by a field stop. `volatile` so a watchpoint is not defeated by a hoisted read. */
    static volatile int total;

    /** A method-exit target that returns the discriminator, so a condition on its frame can select. */
    static int classify(int i) {
        int cd = (i % CYCLE == ODD) ? REAL_FAULT : VALIDATION; // MEXIT_LOCAL — in scope at the return
        return cd;
    }

    static void tick(int i) {
        System.out.println("offer " + i);
        int cd = (i % CYCLE == ODD) ? REAL_FAULT : VALIDATION;
        try {
            throw new AppException(cd);
        } catch (AppException swallowed) {
            // Swallowed, exactly as the target's 30 catch sites do — so the exception stop is the only way
            // to see it at all, which is why the trace budget being spent on noise is the real cost.
        }
        total = cd;
        classify(i);
        System.out.println("done " + i);
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 100000; i++) {
            tick(i);
            System.out.println("tick " + i);
            Thread.sleep(120);
        }
    }
}
