# 0014 — A cassette is keyed by the request, and a miss is loud

## Context

Every MCP-level test in this harness needs a live JVM, so all of them are `#[ignore]`d and need a JDK. Two
consequences followed, and TEST-12 ([#37](https://github.com/YgorPerez/java-debugging-mcp/issues/37)) is
about both.

The first is that **nothing about a real instance can be kept**. TEST-8's ([#24](
https://github.com/YgorPerez/java-debugging-mcp/issues/24)) residue was one sentence — run a dump against
the real 8180 and read the numbers — and the reading evaporates when the session ends. The next question
needs another visit, by someone with access.

The second is that **some shapes cannot be produced at all**. ADR-0011's numbers came from probes;
`FaultRelay` (`7db6318`) reached ADR-0003's honest-failure tail by making one reply lie. But a JVM that
speaks `JDWP 1.5` is not a lie you can inject into one reply — it changes what the debugger *sends* next.
TODO.md's TEST-11 row records the dead end explicitly: JDWP's version tracks the JDK's, the oldest JVM in
the estate speaks 1.11, and so `debug.set_method_exit_stop`'s degraded-arming branch had never executed on
any machine.

The proxy seam was already there twice — `LatencyRelay` and `FaultRelay` — and `7db6318` had written down
the rule for when to merge them: *a third user is the point to unify, not the second.*

## Decision

**Record every request/reply pair through the proxy into a readable file, and serve that file from a port
with no JVM behind it. Key the answers by the request. Answer nothing when there is no match.**

### One proxy seam, two pumps

`Relay` is the socket lifecycle and nothing else: bind, accept the debugger's one connection, dial the
debuggee behind it, keep the live sockets so a blocked `read` can be woken, tear it all down on `Drop`.
Four modes sit on it — latency, fault, record, replay — and what each does with the bytes is a closure.

`target_port` is an `Option`. The replay server is a JDWP **endpoint**, not a middleman, and needs every
line of that except the upstream connect; making the upstream optional is what let the fourth mode reuse
the seam instead of growing a fifth copy of it.

**The pumps did not merge, and that is the decision rather than an omission.** `LatencyRelay` copies raw
chunks and charges its delay per chunk. Those are ADR-0011's numbers, and that ADR documents them as a
*lower bound* precisely because a coalesced read pays the delay once. Framing it — splitting a read into
packets and charging each — would change the instrument, not just its implementation, and would silently
invalidate a published measurement. So `pump_delayed` stays, and the three modes that must understand JDWP
packets share `wire_framed` over a single `read_frames`.

### Keyed by `(command set, command, request payload)`

Not by arrival order. The event pump reads the same socket the commands go down, so the order packets
arrive in is partly a property of the machine's scheduler; a replay that depended on it would be flaky for
reasons unrelated to what is under test. Keying by what was *asked* survives that.

Within one key the recorded answers are served in order, so `AllThreads` before and after a pool grew — one
key, two worlds — can be told apart. Once they run out, the last one repeats: a debugger that polls once
more than the recording did is asking about a world the cassette describes, and failing it would make every
replay depend on retry counts matching to the call.

The keying has a consequence worth stating, because it was immediately load-bearing: **an edit that changes
what the debuggee says can change what the debugger asks.** Making a cassette claim `JDWP 1.5` makes the
debugger arm kind 41 rather than kind 42, so the `EventRequest.Set` key has to move too. A half-done edit
does not pass quietly — it misses, and says which command it missed.

### A miss fails loudly, and never answers

An unmatched request gets **no reply of any kind**. The connection is dropped, the command is named on
stderr with its request payload, the miss is remembered, and `ReplayServer`'s own `Drop` fails the test if
nobody read the log.

This is the criterion the issue put in bold, and it is not a hypothetical: this repo's recurring failure is
a green run of nothing — SIGKILL'd coverage counters, an undetectable JDK, a filter that matched no tests,
a `--` libtest read as a filter. A cassette that answered a miss with `INVALID_OBJECT` would be the next
one, and would make every test built on it worthless while looking fine.

Verified in both directions on a real fixture. Deleting an exchange from the middle fails the assertion with
the command named above it; deleting one from the shutdown traffic — *after* every assertion has already
passed — still fails, through the `Drop` backstop.

### Readable JSON with hex payloads

One object per exchange, `set`/`cmd` as numbers, payloads as hex in 32-byte lines, and each exchange
labelled with its JDWP command name. Written by hand rather than through `serde_json::to_string_pretty`
because a `serde_json::Map` is a `BTreeMap` and would emit the fields alphabetically —
`cmd, command, error, reply, request, set` — which is not the order anyone reads an exchange in. Parsing
goes back through `serde_json` and ignores order, so a hand edit that moves a field is still valid.

The command label is not decoration. Finding "the `ThreadReference.Frames` reply" is what makes an edit
possible; finding "the one with set 11" is not.

### Events are not replayed

A composite event answers no request, so it has no key. Replaying one needs a timer or a cue, and both
invent a time at which the debuggee spoke. The recorder **counts** them, writes the count into the cassette,
and both the recorder and `Cassette::load` say so out loud when it is non-zero.

The issue allowed a first cut to omit events provided it said so rather than half-supporting them. This is
that, stated in three places: the module header, the cassette file, and stderr.

## Rejected alternatives

**Replaying strictly in recorded order.** Simpler, no keys, and it would answer the `AllThreads`-twice case
for free. Rejected on the issue's own reasoning and confirmed by the traffic: the event pump interleaves, so
order is not a property of the debugger alone. It also makes a cassette impossible to edit — inserting one
exchange renumbers everything after it.

**Answering a miss with a JDWP error reply.** The tidy-looking option: the debugger has an error path, the
connection survives, the test reports something. It is the single worst choice available, for the reason in
the criterion — `NOT_IMPLEMENTED` is a perfectly plausible thing for a JVM to say, so the test goes green
having exercised the error branch of whatever it was actually testing.

**Panicking in the replay thread instead.** It is a background thread; a panic there kills the thread, not
the test. The debugger then sees a closed socket and the run fails with something vague and far away. The
miss log plus a `Drop` that fails on the test's own thread says the same thing where it can be read.

**Binary cassettes** — the raw byte stream, or a length-prefixed dump. Smaller, exact, and trivially
written. It fails the criterion that a shape be synthesizable *without re-recording*: a binary fixture is an
unreviewable blob, and the whole point of the JDWP-1.5 cassette is that a reader can see the three fields
that were changed and why. This is the same trade the SMAP fixture already took, and for the same reason —
the harness keeps its fixtures as text next to the thing they are about.

**Base64 rather than hex.** A third shorter. Hex survives a hand edit: one byte is two adjacent characters
at a countable offset, which is what you need to move a length prefix or flip a command kind. Base64 makes
every edit a re-encode.

**Framing `LatencyRelay` so all four modes share one pump.** Would have made the unification total. It
changes a published measurement (see above), which is not a refactor.

**Recording from the real 8180 as part of this change.** Out of scope on the issue and correctly so. This is
the machinery that makes the visit worth taking; the visit is a human step, and the mechanism had to be
complete and tested against probe recordings first.

## Consequences

- **Three tests in `mcp_integration.rs` now carry no `#[ignore]`** and run in the default `cargo test`. That
  file's header used to say every test in it needs a JDK; it says otherwise now, and `stdio_protocol.rs` is
  no longer the only exception.
- **`scripts/integration-test.sh` does not run them.** It passes `--ignored`, which runs *only* ignored
  tests. That is correct — they are not integration tests against a JVM — but it means the cassette tests
  are covered by `cargo test` and not by that script, and a full picture needs both.
- **A cassette is a snapshot and cannot notice the debuggee changing.** It complements the probe suite and
  must not replace it. `list_methods_renders_java_signatures_and_marks_static` and its cassette twin run the
  same assertion body through `disc2_method_listing`, deliberately: the probe test is what would notice a
  real JVM answering differently, and the cassette test is what runs everywhere in two seconds.
- **A cassette is pinned to the JVM it was recorded from**, which is why the file records which one. The
  checked-in DISC-2 fixture was taken from the JBR the box's `Jdk::find` resolves to; replaying it is
  JDK-independent, but re-recording it on a different JVM will change the bytes.
- **Re-recording is opt-in** (`JDWP_RERECORD_CASSETTES=1`), because it needs a JDK and rewrites a reviewed
  artefact. A normal run records into a temporary file and throws it away.
- **`method_exit_on_a_jdwp_1_5_vm.json` has no re-record path at all.** It is not a recording; it is a world
  that was written down, and re-recording would replace it with the world we already have. Its raw material
  is checked in beside it and guarded by an assertion that the debugger still asks for the version.
