# java-debugging-mcp

A native JDWP debugger exposed as an MCP server (Rust). `jdwp-client` speaks the JDWP wire protocol;
`mcp-server` wraps it as `debug.*` MCP tools (attach, breakpoints, stack/variable inspection,
expression evaluation, stepping). See `README.md` for the full tool list and setup.

## Agent skills

### Issue tracker

Issues and PRDs are tracked as GitHub issues (`YgorPerez/java-debugging-mcp`) via the `gh` CLI;
external PRs are not a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary — `needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`.
See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
