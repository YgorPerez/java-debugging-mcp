# java-debugging-mcp — agent router

A native JDWP debugger exposed as an MCP server (Rust). Two crates: `jdwp-client` speaks the wire protocol,
`mcp-server` wraps it as `debug.*` MCP tools.

**Read [`CLAUDE.md`](CLAUDE.md) first.** It is the constraints — the traps that cost time when they are read
late — and it is loaded automatically only by Claude Code. This file exists so a host that does not load it
still finds it.

**Nothing here restates anything.** A second copy of a rule is a second thing to keep in sync, and this repo
has paid for that with a shard number, an ignored-test count and a toolchain pin — see DOC-15 (#145), which
exists because writing a warning next to a number did not stop it going stale.

| you are about to | read |
|---|---|
| anything at all | [`CLAUDE.md`](CLAUDE.md) |
| pick what to read next | [`docs/agents/task-map.md`](docs/agents/task-map.md) |
| use the vocabulary a caller sees | [`CONTEXT.md`](CONTEXT.md) |
| understand why something is the way it is | [`docs/adr/`](docs/adr/) |
| install or run the server | [`README.md`](README.md) |
| set up a dev environment | [`docs/development.md`](docs/development.md) |

## The one thing this host does not get automatically

`scripts/guard.py` holds the traps that are enforced rather than described — `RUSTC_BOOTSTRAP=1 cargo …`, a
`git commit` over a misformatted tree, a soak loop against the working tree, a hardcoded `--shard N/M`.
**Only Claude Code invokes it for you** (LINT-7, #167). Anywhere else it is a command:

```bash
scripts/guard.py check '<the command you are about to run>'   # allow | warn | ask | deny, with the reason
```
