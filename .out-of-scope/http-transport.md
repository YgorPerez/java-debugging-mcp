# HTTP Transport

This server speaks MCP over stdio only. It is spawned as a child process by one MCP client and lives
exactly as long as that client's session. There is no `--transport http`, and adding one is not planned.

## Why this is out of scope

The obvious reason is the weakest one, so take it first and set it aside: an HTTP listener has no trust
boundary, and this server executes arbitrary code inside the debuggee — `debug.evaluate` invokes
methods, `debug.set_value` writes fields, `debug.force_return` skips method bodies, and `JDWP_READONLY`
is documented as a guard against accident and explicitly **not** a security boundary. That is real, but
it is not decisive on its own: anyone who can reach the JDWP port already owns the JVM, so an HTTP
front-end is not categorically worse than the socket it fronts. What makes it worse in practice is
aggregation — one endpoint holding several live sessions, and a listener that invites being exposed in
a way a port-forwarded JDWP socket does not.

The decisive reason is that **client lifetime is currently session lifetime, and the safety model leans
on it.** A stdio server has exactly one client, for exactly as long as the process lives:

- `debug.disconnect` resumes the VM and clears every event request on the way out, so it can never
  leave a shared JVM frozen (SAFE-1).
- Process death is itself a clean, unambiguous signal that the session is over.
- The watchdog exists for the *forgotten stop point* — the case where someone armed something and
  wandered off — not for ordinary disconnection.

Over HTTP none of that holds. A client that closes its laptop mid-suspension leaves the shared 8180
frozen until the watchdog fires. The watchdog would cover it, which is the point of having one, but
"the watchdog is now load-bearing for routine disconnects" is a genuine downgrade in posture rather
than a feature. Deciding what a disconnect *means* — resume? hold? whose session is it? — is the
design problem, and it is not solved by picking a better auth scheme.

EVT-2 (#32) sharpened this. Push notifications assume one client with one lifetime: the notifier arms
on a single `notifications/initialized` and feeds one outbound queue owned by one writer task. With
several clients, "notify the caller" stops having an obvious referent.

```rust
// The single-writer invariant EVT-2 rests on. One channel, one owner, one client:
let (out_tx, mut out_rx) = mpsc::channel::<String>(NOTIFY_CAPACITY);
let writer = tokio::spawn(async move { /* the only thing that writes to stdout */ });
```

## What would reopen this — and the cheaper answer to it

There *is* a real motivation, and it is worth recording so it is not rediscovered as if it were new:
**co-locating the debugger with the debuggee.**

The trace ceiling measured in TRACE-6 (#22) — roughly 720 hits/s, ~1160 with `trace_frames: 0` — is a
serialised round-trip limit. Capture cost is dominated by packet latency on the JDWP connection, so a
debugger running beside the target instead of across a VPN would raise that ceiling materially. This
project already takes the network hop seriously: `LatencyRelay` exists in the test harness precisely to
present a debuggee that behaves like a remote one, and TEST-8 (#24) / ADR-0011 are about calibrating
the shared-instance defaults against realistic latency rather than loopback.

But that motivation argues for *where the process runs*, not for *how the client talks to it*. Running
the existing stdio binary on a host near the JVM — over SSH, or inside the same pod — captures the same
latency win and keeps the one-client-one-lifetime property intact. The remote-debuggee case is already
documented in the README the other way round, forwarding the JDWP port:

```bash
kubectl port-forward pod/my-app-pod 5005:5005
```

So the bar for reopening is not "someone wants HTTP". It is a use case that genuinely needs **more than
one concurrent client** — several people sharing one debug session — together with an answer to what a
disconnect means when the session is not owned by the connection.

Note also that MCP has more than one HTTP-shaped transport and they are not interchangeable; anyone
picking this up should confirm which is current before writing code, or risk building the deprecated one.

## Prior requests

- [#33](https://github.com/YgorPerez/java-debugging-mcp/issues/33) — TRANS-1, filed 2026-07-26 from a
  tool-surface comparison against [`d4n-sec/jdb-mcp`](https://github.com/d4n-sec/jdb-mcp), which offers
  `--transport http` alongside stdio. Filed `needs-triage` rather than `ready-for-agent` because it was
  never specified enough to build; closed after triage established that the motivation behind it has a
  cheaper answer.
