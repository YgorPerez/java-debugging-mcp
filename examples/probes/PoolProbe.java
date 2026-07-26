// Probe for TEST-6 assumption 1 (ThreadOnly under a real thread pool), driven by mcp_integration.rs.
//
//   javac -g PoolProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8821 -cp . PoolProbe
//
// `ThreadProbe` verifies the thread filter against two dedicated, immortal threads. A WildFly request
// pool differs in three ways, and this probe reproduces all three:
//
//   1. HUNDREDS of threads run the same code, so the filter has far more to exclude than one sibling.
//   2. Threads are REUSED across unrelated tasks, so a thread id outlives any one unit of work.
//   3. Threads are RETIRED when idle — so a thread id a filter is pinned to can stop existing while the
//      session is still open, which is routine on a real pool and impossible with a dedicated thread.
//
// Load shape matters, and getting it wrong makes the probe lie. Each task sleeps like a request being
// served, and a whole BATCH is submitted per iteration so the pool stays saturated: threads are reused
// across many tasks rather than idling past their keep-alive and being replaced.
//
// The batch is not stylistic. Two earlier shapes both failed, and both failed *quietly* — the probe ran
// and looked fine while reproducing something other than a loaded pool:
//   - submitting one task every 20ms left 199 of 200 threads idle and churned 300+ threads in 5s;
//   - submitting one task per `Thread.sleep(1)` was meant to fix that, but Windows rounds a 1ms sleep to
//     ~15ms, so the real rate was ~65/s, the pool settled at 55 of 200 threads, and ~500 threads had been
//     created and retired within 10s.
// Batching decouples the load from the platform's timer granularity, so the pool is saturated on any host.
//
// (3) is therefore driven deliberately, not by accident: send `quiesce` on stdin and the probe stops
// submitting for several keep-alives, so every worker times out and dies; send `resume` and the pool
// builds fresh threads with fresh ids. A stop point filtered to a retired thread is then pointed at a
// thread that no longer exists.
//
// main prints the heartbeat — `tick <n> handled=<count> pool=<size>` — rather than each task, because a
// pool this busy would otherwise flood stdout. Which thread ran what is the DEBUGGER's view to report,
// which is the thing under test.
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;

public class PoolProbe {

    // Comfortably "hundreds", and far more than the 40-thread default dump limit.
    static final int POOL = 200;

    // Short enough that a quiesce retires the whole pool in a few seconds, long enough that a busy
    // worker is never retired between two tasks.
    static final long KEEP_ALIVE_MS = 800;

    // How long a task "serves a request" for.
    static final long TASK_MS = 150;

    // Tasks submitted per iteration, and the gap between iterations. `BATCH` tasks every `TASK_MS / 2`,
    // each lasting `TASK_MS`, means about `2 * BATCH` are in flight — enough to keep all `POOL` threads
    // busy continuously, without depending on how precisely the host honours a short sleep.
    static final int BATCH = POOL / 2;

    static final AtomicInteger handled = new AtomicInteger();
    static final AtomicBoolean quiesced = new AtomicBoolean(false);

    // A dedicated type, so an exception breakpoint can target it precisely.
    static class PoolException extends RuntimeException {
        PoolException(String m) {
            super(m);
        }
    }

    // The traced site: throws and swallows in one place, so an exception breakpoint has a stable throw
    // site and a line breakpoint a stable line. Runs on whichever pool thread picked the task up.
    static void doWork(int i) {
        handled.incrementAndGet();
        try {
            throw new PoolException("task-" + i); // BP1
        } catch (PoolException e) {
            // swallowed on purpose
        }
    }

    static ThreadFactory namedFactory() {
        return new ThreadFactory() {
            private final AtomicInteger n = new AtomicInteger();

            @Override public Thread newThread(Runnable r) {
                // A fresh number per thread, so a retired-and-replaced worker is visibly a different
                // thread rather than looking like the same one reused.
                Thread t = new Thread(r, "pool-worker-" + n.incrementAndGet());
                t.setDaemon(true);
                return t;
            }
        };
    }

    static void startCueReader() {
        Thread cues = new Thread(() -> {
            try (java.io.BufferedReader in =
                         new java.io.BufferedReader(new java.io.InputStreamReader(System.in))) {
                String line;
                while ((line = in.readLine()) != null) {
                    // Matched by `contains`, not `equals`: a writer that prepends a UTF-8 BOM (PowerShell
                    // does) would otherwise send "﻿quiesce" and the cue would be silently ignored,
                    // leaving the probe looking healthy while reproducing nothing. Cost of being lax here
                    // is nil — these are the only two cues.
                    if (line.contains("quiesce")) {
                        quiesced.set(true);
                    } else if (line.contains("resume")) {
                        quiesced.set(false);
                    }
                }
            } catch (Exception e) {
                // stdin closed; no cues left to read
            }
        }, "cue-reader");
        cues.setDaemon(true);
        cues.start();
    }

    public static void main(String[] args) throws Exception {
        ThreadPoolExecutor pool = new ThreadPoolExecutor(
                POOL, POOL, KEEP_ALIVE_MS, TimeUnit.MILLISECONDS,
                new ArrayBlockingQueue<>(2000), namedFactory(),
                // Shed load rather than blocking the submit loop, so a full queue can never stall the
                // heartbeat that proves the probe is alive.
                new ThreadPoolExecutor.DiscardPolicy());
        // Core threads must be allowed to die, or (3) is unreachable: a fixed pool that never retires
        // anyone cannot reproduce a filter pinned to a thread that stops existing.
        pool.allowCoreThreadTimeOut(true);
        pool.prestartAllCoreThreads();
        startCueReader();

        boolean wasQuiesced = false;
        long lastBeat = 0;
        for (int i = 0; i < 100000000; i++) {
            if (quiesced.get()) {
                if (!wasQuiesced) {
                    // Several keep-alives, so every idle worker is certainly past its timeout.
                    Thread.sleep(KEEP_ALIVE_MS * 5);
                    System.out.println("quiesced pool=" + pool.getPoolSize());
                    wasQuiesced = true;
                }
                Thread.sleep(100);
                continue;
            }
            if (wasQuiesced) {
                System.out.println("resumed pool=" + pool.getPoolSize());
                wasQuiesced = false;
            }

            for (int b = 0; b < BATCH; b++) {
                final int n = i * BATCH + b;
                pool.submit(() -> {
                    doWork(n);
                    try {
                        Thread.sleep(TASK_MS); // serving the request
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                    }
                });
            }

            // Heartbeat on a timer rather than per task: at this submission rate, one line per task
            // would be thousands a second.
            if (System.currentTimeMillis() - lastBeat >= 500) {
                System.out.println("tick " + i + " handled=" + handled.get() + " pool=" + pool.getPoolSize());
                lastBeat = System.currentTimeMillis();
            }
            Thread.sleep(TASK_MS / 2);
        }
    }
}
