# Development

Building, testing and the lint gate. For what the tools *do*, see [`tools.md`](tools.md).

## Architecture

```
Claude Code → MCP Server → JDWP Client → TCP Socket → JVM
                ↓
         Summarization &
         Context Filtering
```

The MCP server handles:
- **Protocol Translation**: MCP JSON-RPC ↔ JDWP binary protocol
- **Smart Summarization**: Truncates large objects, limits depth
- **State Management**: Tracks breakpoints, threads, sessions

## Project structure

```
jdwp-mcp/
├── jdwp-client/        # JDWP protocol implementation
│   ├── connection.rs   # TCP + handshake
│   ├── protocol.rs     # Packet encoding/decoding
│   ├── commands.rs     # JDWP command constants
│   ├── types.rs        # JDWP type definitions
│   └── events.rs       # Event handling
├── mcp-server/         # MCP server
│   ├── main.rs         # Stdio transport
│   ├── protocol.rs     # MCP JSON-RPC
│   ├── handlers.rs     # Request routing
│   ├── tools.rs        # Tool definitions
│   ├── session.rs      # Debug session state
│   └── tests/          # MCP-level integration tests (real binary + real JVM)
└── examples/
    ├── test_*.rs       # jdwp-client protocol examples
    └── probes/         # Java programs the tests and examples attach to
```

## Testing

```bash
cargo test                      # unit tests + the stdio protocol tests (fast, no JVM)
scripts/integration-test.sh     # MCP-level: the real binary over JSON-RPC against probe JVMs
scripts/doctor.sh               # the rust-doctor health gate CI runs
```

`scripts/integration-test.sh` runs `mcp-server/tests/mcp_integration.rs`, which launches and reaps its
own probe JVMs from `examples/probes/` — no manual steps. It does need a JDK: without one every test
prints `SKIP` and passes, so check for `SKIP` lines before reading a green run as coverage.

Which JDK it used is printed once per run and repeated as the last line, because a green run that cannot
be attributed to a version is worth less than it looks (TEST-18,
[#52](https://github.com/YgorPerez/java-debugging-mcp/issues/52)):

```
JDK in use: javac 11.0.30 at /home/you/.jdks/ms-11.0.30 (found via JAVA_HOME)
```

With `JAVA_HOME` unset the harness searches `PATH` and then a snap-installed IntelliJ's bundled runtime,
and the banner says which it settled on. Setting `JAVA_HOME` is a **request for that specific JDK**: if it
is not a usable one — a JRE with no `javac`, most often — the run fails and names what was missing rather
than quietly testing a different JVM, which is what it used to do.

`mcp-server/tests/stdio_protocol.rs` is one exception: it drives the real binary's JSON-RPC front door
with malformed input (unparseable lines, non-objects, missing `method`, EOF mid-message) and needs no JDK,
so it runs in plain `cargo test`. Each case checks that an error came back **and** that the server is
still serving afterwards, since one bad line from a client must not end the session.

The **cassette** tests are the other (see below). They live in `mcp_integration.rs` but carry no `#[ignore]`
— which means `scripts/integration-test.sh` does *not* run them, since `--ignored` runs only ignored tests.
Both commands are needed to see the whole file.

#### Recorded sessions: testing the debugger with no JVM at all

A third proxy mode **records** every JDWP request/reply pair to a file, and a replay server answers from
that file with nothing behind the port (ADR-0014, TEST-12
[#37](https://github.com/YgorPerez/java-debugging-mcp/issues/37)):

```bash
cargo test --test mcp_integration list_methods_renders_java_signatures_from_a_cassette   # no JDK needed
JDWP_RERECORD_CASSETTES=1 scripts/integration-test.sh a_recorded_session_replays          # re-record
```

The cassettes are in `mcp-server/tests/cassettes/` and are meant to be read and edited: JSON, one object
per exchange, payloads as hex in 32-byte lines, each exchange labelled with its JDWP command name. Answers
are keyed by `(command set, command, request payload)` rather than by arrival order, and **a request the
cassette cannot answer gets no reply at all** — the connection drops, the command is named on stderr, and
the test fails. A replay that quietly returned an error reply would make every test using it meaningless.

Two things this buys that a probe cannot:

- **One visit to a real instance becomes a permanent fixture.** Record once, replay forever, with no
  access, no JDK and no JVM.
- **Shapes nothing here can produce become testable by editing a file.**
  `method_exit_on_a_jdwp_1_5_vm.json` is a hand edit of a five-exchange recording that makes the debuggee
  answer `JDWP 1.5`, which reaches `debug.set_method_exit_stop`'s degraded arming — a branch a JDK matrix
  cannot reach, because JDWP's version tracks the JDK's and the oldest JVM in the estate speaks 1.11.

Events are **not** replayed: a composite event answers no request, so it has no key. The recorder counts
them and writes the count into the cassette, and says so when it is non-zero.

#### Testing shared-instance behaviour without a shared instance

The costs that matter on a busy remote JVM — how long a dump freezes it, how much a trace slows it — used
to be answerable only against the real thing. They aren't. Three variables separate a real app server from
a loopback probe, and two of them belong to the debuggee:

| variable | how a test presents it |
| --- | --- |
| hundreds of threads, not tens | `PoolShapeProbe` — 300 workers, named like a real pool |
| stacks far deeper than `max_frames` | `PoolShapeProbe` — 60 **distinct** frames per worker |
| a network hop instead of loopback | `LatencyRelay::start(probe.port, rtt)`, then attach to `relay.port` |

`LatencyRelay` forwards the JDWP stream adding a measured round trip, in userspace — `tc … netem` needs
`NET_ADMIN`, and deterministic latency beats a real network's jitter for a test. It charges coalesced
traffic once, so measurements through it are a lower bound, and it models latency only.

The round trip is a **dial** (`relay.set_rtt(rtt)`), not just a constructor argument, and a test that
*compares* two latencies should use it rather than standing up a second relay. Two relays mean two
attaches, which puts a JVM handshake and several seconds between the readings — long enough on a box
running the rest of this suite for a load spike to land on one of them and not the other, which is
indistinguishable from the wire. Turning the dial under one live connection, alternating the arms and
scoring each on its *fastest* sample, puts both readings in the same few seconds of the same machine
(TEST-13, [#38](https://github.com/YgorPerez/java-debugging-mcp/issues/38)).

The cost model these established was `held ≈ packets × (our per-packet cost + RTT)`, measured linear in
RTT with a slope of 1 packet per round trip. **PERF-1 (#100) amended it** — independent reads now share a
round trip, so the model is `held ≈ round_trips × RTT + packets × our processing`, and on a remote
instance the **round-trip count** is the figure to reason about (ADR-0038). Packets still bound it, which
is why a dump caches line tables per call (ADR-0011) rather than being given a longer suspension budget.

Assert packet counts, not durations: a packet count is deterministic and load-independent.

You do not have to take any of these figures on trust against your own instance, either: a dump reports
what **it** cost there —

```
🧵 Thread dump — 40/306 thread(s)
   ⏱  Held the VM suspended for 779ms.
Cost: 258 JDWP packet(s) in ~61 round trip(s) — independent reads share one, so the packet count above
is what crossed the wire and this is what was waited for, 3.08ms each.
```

The round-trip clause **suppresses itself when the two numbers are equal**, so a call that overlapped
nothing reads as it did before PERF-1 — the clause appearing *is* the information that something shared a
trip.

— and a dump the budget truncated says what finishing would have taken at the rate it was running, so the
choice between narrowing it and raising `max_suspend_ms` is made against a number rather than a guess.
Measured with the relay, the defaults hold the VM inside the 2000 ms budget up to roughly a **6 ms round
trip**; past ~7 ms even a defaults dump truncates, which is the safety net working.

For poking at the tools by hand against a realistic app, use the companion `java-example-for-k8s`
checkout as a target — a sibling directory of this repo, not a submodule, so it may not be present:

```bash
cd ../java-example-for-k8s   # from this repo's root
mvn clean package
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005 \
  -jar target/probe-demo-0.0.1-SNAPSHOT.jar
```

## Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test
```

## Code health

[rust-doctor](https://github.com/arthjean/rust-doctor) folds clippy, `cargo-audit`/`deny`/`geiger`,
and custom AST rules into one 0–100 score. Run it locally (no Rust build of the tool — `npx` fetches
a prebuilt binary):

```bash
scripts/doctor.sh              # score card for the workspace
scripts/doctor.sh --findings   # the findings the gate counts, and whether it would pass
scripts/doctor.sh --verbose    # per-finding file:line detail
scripts/doctor.sh --diff main  # only files changed vs main
```

**The score is not the gate.** CI fails the build on any warning, so a 100/100 "Great" can still be a
red build — v0.2.0's tag build was, on five `excessive-clone` findings that had been sitting in a local
run nobody could read out of it. `--findings` prints each warning/error in the same shape the CI step
summary uses, says whether the gate would pass, and exits 3 if it would not:

```
- **warning** `excessive-clone` — `src/handlers.rs:3233`
  `.clone()` inside a loop — may cause repeated heap allocations
```

It also names what the run did *not* look at — passes skipped for a missing tool, and passes that ran
only because you have a tool CI does not install — since either one moves the verdict away from CI's.

The same check runs in CI (`.github/workflows/rust-doctor.yml`, pinned to 0.2.0): it **gates on
warnings** — a finding fails the build (#18) — and uploads results to GitHub code scanning (SARIF).
Installing the optional external tools (`cargo install cargo-audit cargo-deny cargo-machete
cargo-geiger`) unlocks the dependency/unsafe passes it otherwise skips.

Because it gates on warnings, the Rust toolchain there is pinned, so a new pedantic lint in a future
clippy cannot break a build on code nobody touched. `.github/workflows/toolchain-pin.yml` runs the same
scan against `stable` once a month **without gating**, and opens an issue when the pin is behind — the
bump is scheduled work rather than a surprise. See ADR-0007.

One `clippy.toml`, at the workspace root, covers every crate; adding a workspace member needs nothing.
It only applies because `scripts/doctor.sh` and the workflows set `CLIPPY_CONF_DIR` — rust-doctor drops
a temporary `clippy.toml` into any member that lacks one, which would otherwise shadow it. The file
says the rest.

## Serena (semantic code navigation for agents)

[Serena](https://github.com/oraios/serena) is registered as an MCP server for this repo, giving an agent
symbol-level navigation over the Rust workspace instead of grep-and-read. The repo carries the shared
configuration; each machine needs a one-time install.

**One-time setup:**

```bash
# uv (Serena is a Python tool), then Serena itself
winget install astral-sh.uv            # or: curl -LsSf https://astral.sh/uv/install.sh | sh
uv tool install -p 3.13 serena-agent
serena init

# Rust support uses rust-analyzer from your rustup toolchain
rustup component add rust-analyzer

# Build the symbol cache once (a few seconds; it is gitignored)
serena project index .
```

Committed here, so nothing else is needed: `.mcp.json` (the server registration, using
`--project-from-cwd` so it contains no absolute paths), `.serena/project.yml` (Rust only — the Java files
under `examples/probes/` are fixtures and get no language server), and `.claude/settings.json`
(Serena's hooks).

**One thing worth knowing before you rely on it**, measured on this workspace by tracing the LSP traffic:

**Semantic queries return empty for the first ~2.5 minutes of a session, then work correctly.**

rust-analyzer signals `quiescent` after about **152s** here — it spends that time on `Fetching`,
`Building compile-time-deps`, `Building CrateGraph` and `Loading proc-macros` for the dependency tree.
Serena stops waiting at a **hard-coded 120s** (`_SERVER_READY_TIMEOUT` in its `rust_analyzer.py`) and
proceeds anyway, so a query in that ~30s gap is sent to a server that is not ready: rust-analyzer answers
`[]` and the tool reports `{}`.

What that means in practice:

| | behaviour |
| --- | --- |
| `find_symbol`, `get_symbols_overview` | work immediately — document symbols only need parsing |
| `find_referencing_symbols` and other semantic queries | empty before ~152s, **correct after** |
| after quiescence | ~30ms–3s per query, including cross-crate references |

**Raising the wait fixes it**, and is worth doing: at the default the first semantic query burns two
minutes *and* returns an empty result, whereas with a longer wait it takes ~152s and is correct.

The limit is a hard-coded local in Serena (`_SERVER_READY_TIMEOUT = 120.0` in
`solidlsp/language_servers/rust_analyzer.py`) with no env var or config key, so it takes a one-line patch
to make it configurable:

```bash
scripts/serena-ready-timeout.sh            # apply (rewrites the constant to read an env var)
scripts/serena-ready-timeout.sh --check    # report status; exit 1 if not applied
scripts/serena-ready-timeout.sh --revert   # restore the original line
```

It keeps `120` as the default and reads `SERENA_RUST_READY_TIMEOUT`, which `.mcp.json` sets to `300` for
this repo. It is idempotent and refuses to run if the upstream line has changed — **re-run it after
`uv tool upgrade serena-agent`**, which replaces the file. `--check` is a useful thing to run if semantic
queries start coming back empty again.

Without the patch, nothing is broken; just re-run a query that came back empty.

Two other setup notes:

- **`export MCP_TIMEOUT=300000`.** Serena's docs suggest `60000`; that is not enough here.
- **Don't conclude "no references" from an early empty result.** That is the one genuinely misleading
  behaviour, and it is a timing artefact rather than a limitation.

Tuning rust-analyzer instead was measured and does not help: disabling `cachePriming` and `check` saves
only ~5s of the 152s, and the settings that *would* help (`procMacro.enable: false`,
`buildScripts.enable: false`) would break analysis of the derive macros this codebase is full of.

Serena's own docs note that Claude Code's built-in tool descriptions bias the model strongly toward
internal tools. The committed hooks are their recommended mitigation; they also suggest launching with

```bash
claude --system-prompt="$(serena prompts print-cc-system-prompt-override)"
```

which is left to you, since it changes how you start Claude Code rather than anything in this repo.

Serena's **memories are deliberately not versioned** (see `.gitignore`). This repo keeps its curated
knowledge in `CONTEXT.md`, `docs/adr/` and `TODO.md`; an agent-written store beside those would give the
same facts two sources of truth. `.serena/project.yml`'s `initial_prompt` points Serena at those files
instead.
