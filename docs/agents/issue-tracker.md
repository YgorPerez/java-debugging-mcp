# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues. Use the `gh` CLI for all operations.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering comments by `jq` and also fetching labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply / remove labels**: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- **Close**: `gh issue close <number> --comment "..."`

Infer the repo from `git remote -v` — `gh` does this automatically when run inside a clone.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external PRs as feature requests; `/triage` reads this flag.)_

When set to `yes`, PRs run through the same labels and states as issues, using the `gh pr` equivalents:

- **Read a PR**: `gh pr view <number> --comments` and `gh pr diff <number>` for the diff.
- **List external PRs for triage**: `gh pr list --state open --json number,title,body,labels,author,authorAssociation,comments` then keep only `authorAssociation` of `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, or `NONE` (drop `OWNER`/`MEMBER`/`COLLABORATOR`).
- **Comment / label / close**: `gh pr comment`, `gh pr edit --add-label`/`--remove-label`, `gh pr close`.

GitHub shares one number space across issues and PRs, so a bare `#42` may be either — resolve with `gh pr view 42` and fall back to `gh issue view 42`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.

## How to validate anything here (the house pattern)

Salvaged out of `TODO.md` when that file was deleted, and kept because the `/to-issues` vertical-slice format
below is what issues here are written in: each item is a complete end-to-end capability — JDWP primitive(s) in
`jdwp-client` + wiring/tool in `mcp-server` + a validation against a live probe — not a horizontal layer.

Two layers, two mechanisms. Pick by what the change touches.

**MCP-server behaviour (handlers, event pump, session state) — an integration test.**
These drive the real `jdwp-mcp` binary over JSON-RPC on stdio against a real probe JVM, and they run
from `cargo test`:

```
scripts/integration-test.sh              # all of them
scripts/integration-test.sh force_return # filter by test name
```

Add cases to `mcp-server/tests/mcp_integration.rs`; the harness (`tests/common/mod.rs`) compiles and
launches the probe, picks a free port, reaps the JVM, and captures the probe's stdout+stderr. Write a
probe under `examples/probes/` and mark breakpoint lines with `// BP<n>` comments — tests locate them
by marker, so editing the Java can't silently point a test at the wrong statement.

They are `#[ignore]`d (they spawn JVMs and need a JDK), which is why the runner passes `--ignored`.
**With no JDK every test prints `SKIP` and passes** — so a green run on a JDK-less machine proves
nothing; grep the output for `SKIP`.

**Raw `jdwp-client` protocol work — an example.**
Register an `examples/test_*.rs` in `jdwp-client/Cargo.toml` and run it against a hand-launched probe:

1. Compile the probe with `-g` (without it there is no local-variable table, so no locals can be
   read at all). No system JDK on this box — the JBR at `/snap/intellij-idea-ultimate/*/jbr/bin/javac`
   is the only one, which is also why the test harness looks there.
2. `java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:<port> -cp . Probe`
   (dt_socket `server=y` accepts ONE connection then stops listening — fresh port per run).
3. `cargo run --release --example …`

Worked patterns: `examples/test_static_field.rs`, `examples/test_deferred_bp.rs`.

**Shared-instance behaviour — a shaped probe plus the latency relay. You do not need the 8180.**
This was the standing excuse for leaving the shared-instance defaults uncalibrated, and it does not hold.
What makes a real app server different from a loopback probe is three variables, and two of them belong to
the *debuggee*:

| variable | how to present it here |
| --- | --- |
| hundreds of threads, not 60 | `PoolShapeProbe` — `WORKERS = 300`, named like a real pool's |
| stacks far deeper than 8 frames | `PoolShapeProbe` — `DEPTH = 60`, **distinct** methods, not recursion |
| threads in DIFFERENT code, not one shape | `MixedPoolProbe` — 300 workers across 10 handlers over a shared 40-frame framework prefix |
| a network hop instead of loopback | `LatencyRelay::start(probe.port, Duration::from_millis(4))`, then attach to `relay.port` |

`LatencyRelay` (in `tests/common/mod.rs`) forwards the JDWP stream with a delay per chunk. Userspace
because `tc qdisc … netem delay` needs `NET_ADMIN`, which a container lacks — and because deterministic
latency is better for a test than a real network's jitter. It charges coalesced traffic once, so a
measurement through it is a **lower bound**; it models latency only, not loss or bandwidth.

**Comparing two latencies: dial, don't restart.** `relay.set_rtt(rtt)` moves the round trip on a live
connection. Standing up a second relay instead means a second attach, and the JVM handshake between the
two readings is long enough for a load spike to hit one and not the other — which reads exactly like the
wire. Alternate the arms on one connection and score each on its *fastest* sample: a busy box can only
make a dump slower, so the floor of a few samples is the cost with the noise removed, and it is a floor
for both arms alike. See TEST-13 below.

Depth must be **distinct methods**. Line tables are cached per dump by `(class, method)`, so a recursive
chain collapses to one lookup and flatters the cache; a real request stack has about as many methods as
frames (ADR-0011).

Prefer asserting **packet counts** over durations for anything cost-related: a packet count is
deterministic and independent of machine load, a duration is neither. See
`a_production_shaped_dump_costs_a_bounded_number_of_packets_per_thread`, whose ≤20-per-thread bound fails
at ~70 if the cache is removed — verified by defeating it, not assumed.

What still needs the real instance is only its **own parameters**, and a dump now reports them: one
`debug.thread_dump` at defaults gives the thread count in its header and the per-packet cost (hence the RTT)
on its cost line, and a truncation states what finishing would have taken. No `ping`, no arithmetic. The
figures below are what to compare that reading against.

Note: the `mcp__jdwp__` tools in a running Claude Code session hold the OLD binary — a rebuild is only
picked up after a Claude Code restart. That's why validation goes through tests/examples, not the live
tools, within a session. The integration tests sidestep this entirely: they spawn the binary Cargo just
built for that run (`CARGO_BIN_EXE_jdwp-mcp`), so they can never test a stale binary.

---

