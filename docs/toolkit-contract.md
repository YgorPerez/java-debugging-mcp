# The contract with infotravel-dev-toolkit

This server has a downstream consumer:
[ygor-infotera/infotravel-dev-toolkit](https://github.com/ygor-infotera/infotravel-dev-toolkit), which
installs `jdwp-mcp` onto other people's machines and documents its tools in Claude Code skills. This file is
the **upstream side** of that coupling: what shipping a change here does over there. The other side is
`docs/jdwp-contract.md` in that repo; the two are meant to be read as one thing.

## Nothing here depends on the toolkit

This repo builds, tests and releases with no knowledge of it. Nothing in the toolkit can break this CI, and
it must never be a reason a change cannot ship. **The coupling is entirely one-way and entirely
documentary** — which is exactly why it needs writing down. A dependency with no build-time link does not
fail when it drifts; it just stops being true, quietly, on other people's machines.

Concretely: this repo owes the toolkit **no compatibility guarantee**. Rename a tool if renaming it is right
(VOCAB-1/#20 renamed seven). What it owes is *legibility* — a release whose caller-visible changes can be read
off the release notes without diffing.

## What the toolkit consumes

Only three things, and only from a release:

1. **A git tag `vX.Y.Z`**, which triggers `.github/workflows/release.yml`.
2. **Assets named `jdwp-mcp-<tag>-<os>-<arch>`** plus `SHA256SUMS`.
3. **`SHA256SUMS` as the manifest** — the toolkit matches the host's asset name against its contents rather
   than assembling a URL, so it refuses rather than guesses.

It pins one tag in `JDWP_VERSION`. It does not track `main`, so **unreleased work is invisible to it**:
a tool that exists only on `main` cannot be documented downstream without advertising something nobody can
call.

The asset **naming** is therefore part of the interface, not an implementation detail of the workflow.
`release.yml` already guards this from its own side — it fails the publish if anything in `dist/` is not a
platform binary, which is what REL-2 (#41) taught it after a SARIF file shipped as a release asset. That
guard protects the toolkit too.

## What a change here costs downstream

Ordered by how *quietly* it breaks, because that is what decides whether it needs a note in the release:

| Change here | Downstream effect | Noticed? |
|---|---|---|
| Rename or remove a **tool** | Skill docs name a tool that no longer exists | **No.** Fails at use time, weeks later. `set_breakpoint` outlived VOCAB-1 in their docs |
| Rename a **tool argument** | Documented examples silently wrong | **No** |
| Change what a **reply says** | Prose quoting the reply goes stale | **No** |
| Add a **tool** | Nothing breaks; it simply goes undocumented and unused | **No** — nobody finds a tool the docs do not name |
| Change **behaviour behind an existing name** | Docs describe the old behaviour | **No** — the worst case, since the docs still look right |
| Corrupt or truncate a **tool description** | The description *is* the caller's documentation, and the toolkit's skills paraphrase it. Gibberish, or a capability silently deleted from it, propagates as advice | **No** — and it happened: DOC-7 (#108) |
| Rename an **asset** or drop `SHA256SUMS` | Their install refuses | **Yes**, immediately |

Six of seven are silent. So the rule is not "avoid breaking them" — it is **say what changed, in the release
notes, in caller-visible terms**.

**PERF-1 (#100) is the "Change what a reply says" row, and it is the mild version of it.** A dump's cost line
now reads `Cost: 763 JDWP packet(s) in ~180 round trip(s), 1.54ms each` where it used to read
`Cost: 763 JDWP packet(s), 4.89ms each`. Nothing was renamed and nothing was removed — a clause was added
between two things that were already there — so prose quoting the packet count still holds and prose quoting
the per-packet figure now reads a smaller number for the same work.

That smaller number is the thing to state, because it is easy to read as a regression in reverse: the dump got
**faster** (the same 763 packets, held 3731ms → 1175ms at a 4ms round trip), and the per-packet price fell
because packets stopped being waited on one at a time. Anything downstream that quotes "~0.2ms per packet on
loopback" or reasons that `held ≈ packets × RTT` is now working from the wrong model; the release notes have to
say `held ≈ round_trips × RTT + packets × our processing`, and that the round trips are the figure to reason
about on a remote instance. `CONTEXT.md`'s `Packet` entry and ADR-0038 carry the same amendment.

**`debug.run_named_query` (EVAL-11, #124) is the "Add a tool" row being exercised, and that row is silent.**
The toolkit will install a binary that can run a named JPA query and its skills will not mention it, so
nobody will call it — the tool is not broken, it is invisible. Two things follow. The release notes have to
name it in caller-visible terms, including the parts a caller has to know before using it: it INVOKES so it
needs an event-suspended thread and a read-only session refuses it, it suppresses the query's flush and says
what that costs in accuracy, and with no `EntityManager` in the frame it refuses with a two-step rather than
searching the heap. And `jdwp-trace`'s swallowed-exception playbook is the skill this belongs in downstream —
"a motor call returns null" is on its trigger list and "the query matched the whole table" is the same bug
seen one layer down.

The description row is the one that arrived by accident rather than by decision, and it is worth reading twice
for that reason. Two merges in the v0.9.0 range interleaved `debug.evaluate` and `debug.evaluate_chain`'s
descriptions — each is a single ~4000-character string literal, so git had nothing to conflict *on* — and
shipped `"Anees n thread at all."`, `"invokhing"`, and the `@0x…` object-handle capability **deleted from both
head lists** with ungrammatical fragments about it stranded at the end. Nothing failed: the tests asserted on
tool names and nothing else, and no human reads a 4000-character line in a diff.

That last sentence used to say the tests asserted on "tool names and argument shapes". They did not, and the
same over-statement sat in the code comment it was copied from (DOC-8, #120) — which mattered, because a reader
deciding whether an argument change needed a guard would conclude one was already there. It is true now.

So a description change is gated two ways, and `mcp-server/tests/tool-descriptions.txt` is the important one:
a word-wrapped snapshot of all 38 descriptions that a deliberate edit must regenerate. That regeneration step
**is** the review moment this table asks for — the point at which somebody reads what a caller will read.

**`mcp-server/tests/argument-schemas.txt` is the same guard for the row above it** — the one about renaming an
argument, which is silent too. It is generated from the advertised tool list rather than a hand-kept roster, so
it covers **38 tools and 184 arguments** by construction: each argument's full schema minus its description
(type, default, `format`, `minimum`, `anyOf`, whether it is required) followed by the description word-wrapped.
`schemars` publishes those descriptions as the `inputSchema`, so they are the caller's documentation for every
argument — the same thing the tool description is one level up, and this file's insistence that an audit count
**arguments** rather than tools is the reason it exists. It caught nothing before it was written because it did
not exist: five argument descriptions changed in v0.14.1 and the suite showed no diff at all. Both snapshots
regenerate with the one command, `UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot`.

## What to do when cutting a release

`scripts/release.sh` prints the reminder at the end for this reason. The release commit body is the artifact
that matters, and **since v0.9.0 it is the artifact that actually gets published**: `scripts/release-notes.py`
leads the release body with that commit's message body verbatim, then appends a changelog categorized from the
conventional-commit subjects since the previous tag.

That sentence used to read `gh release create --generate-notes` builds the notes from commits, and it was wrong
in the way that cost the most: `--generate-notes` lists merged **pull requests**, not commits, and never reads
a commit body at all. Plenty of work here lands as a direct push to `main`, so every release from v0.2.1 to
v0.8.0 published exactly one line — the compare link — while this file was pointing the toolkit at release
notes as the one mitigation for six silent failure modes. Preview what will actually be published with
`python3 scripts/release-notes.py v<version>`; it is byte-for-byte the result.

For each caller-visible change, the release body should name:

- the **tool** affected, by its exact current name;
- whether an **argument** was added, renamed or removed;
- whether a **reply** changed shape or wording, if downstream prose is likely to quote it;
- whether a **tool description** changed, and how — the description is what the toolkit's skills paraphrase, so
  a corrected or expanded one is a caller-visible change even though no name, argument or reply moved. The diff
  to read is `mcp-server/tests/tool-descriptions.txt`, which is why it is committed;
- whether an **argument's** type, default or description changed — the diff to read is
  `mcp-server/tests/argument-schemas.txt`, committed for the same reason;
- for a rename, **both** names — the toolkit's audit greps for old names, and that only works if the release
  says what the old name was.

`v0.5.0`'s body is the intended shape: three new tools named, read-only's widened refusal stated, and the
residue reporting described in terms of which tools now report it.

## The failure mode this file exists to prevent

Shipping a *behavioural* change under an unchanged tool name and description. Both repos' documentation then
describes the old behaviour and nothing anywhere disagrees — no test fails, no install refuses, no tool call
errors.

It has already happened once, inside this repo: SWAP-2 (#61) taught `debug.disconnect`,
`debug.list_sessions` and `debug.panic` to report classes left hot-reloaded, and updated none of their tool
descriptions. Those shipped in `v0.5.0` describing behaviour the binary no longer had, and ADR-0015 states
the rule that was broken — **the tool description IS the interface** for an LLM caller. If a description can
go stale inside the repo that changed the code, a skill in another repo has no chance.

So: when behaviour changes, the tool description changes in the same commit. That is what keeps the
downstream audit possible at all, since the description is the only thing about behaviour that a released
binary carries with it.
