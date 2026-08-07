# What to read, for what you are about to do

`CLAUDE.md` is always loaded and holds the constraints. This is the second layer: a bounded set of durable
references per task, so a session about one thing does not carry the catalogue for the other five.

**The third column matters as much as the second.** Naming what to skip is what makes this a route rather
than a reading list — and every row's skip column is a real one, not padding.

| you are about to | read, in this order | skip unless the change reaches it |
|---|---|---|
| **change a tool's behaviour or reply** | `docs/toolkit-contract.md` (six of seven breakages are silent) → `CONTEXT.md` for the vocabulary → the tool's ADR | sharding, the release path |
| **add or rename a tool** | `docs/toolkit-contract.md` → `docs/agents/releasing.md` (it is caller-visible) → `CONTEXT.md` before you pick the name | the lint gate |
| **add a JDWP command to `jdwp-client`** | ADR-0001 → `WIRE_COMMANDS` in `connection.rs`'s test module | everything else; this one is a table entry, not a design |
| **work a flake** | `docs/agents/testing.md` § *Working a flake* → the open flake issues (#45, #56, #64, #71, #118) | the release path, the gate |
| **change a workflow** | `docs/agents/ci.md` → the workflow file's own header, which carries the same reasoning at each line | `CONTEXT.md` |
| **change a gate tool** | `docs/agents/lint-gate.md` → `scripts/doctor.sh` → `mcp-server/tests/docs_claims.rs` | the suite's internals |
| **cut a release** | `/release` (`.claude/commands/release.md`) → `docs/agents/releasing.md` → `docs/toolkit-contract.md` | the suite's internals |
| **change the guard's rules** | `scripts/guard.py` → `.claude/settings.json`'s comment block (the one place the rationale lives) → `scripts/guard.test.sh` | everything else |
| **set up a clone, or the git hooks** | `docs/development.md` — it owns the hooks, the toolchain and Serena | everything else |
| **change the `trace_expr` session default** | ADR-0040 → `CONTEXT.md`'s **Session default** entry → `handlers.rs` | the gate, the release path |
| **change the docs** | ADR-0039 (why the prose is long) → `docs_claims.rs` → this file | the code |
| **triage or file an issue** | `docs/agents/issue-tracker.md` → `docs/agents/triage-labels.md` | the code |

## The rule behind the table

**Each fact has one authored source.** Where a fact appears twice, one of the two is a copy that will rot —
this repo has paid for that with a shard number, an ignored-test count and a toolchain pin, and
`docs_claims.rs` exists because writing a warning next to the number did not work.

So a route points at the source. It does not summarise it, and neither does `CLAUDE.md` or `AGENTS.md`.

## What is deliberately not here

**No machine-readable manifest of these routes, and no CI check for unclassified documents.** Both are the
right end state for a repo with many contributors. Here they would be a second artefact to keep in sync in a
repo where `docs_claims.rs` already asserts the claims that have cost something — so this lands first, and
whether the drift justifies the manifest is a question to answer with evidence rather than in advance. Same
call CI-5 made about gating zizmor before knowing the count.

**No path → review-lens table.** Worth having, and it is `/code-review`'s input rather than a router's.
Follow-up.
