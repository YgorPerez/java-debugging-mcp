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

### Available and deliberately not consumed

A release also publishes **`tool-surface-<tag>.json`** (REL-8, #165): every `debug.*` tool, its description
and every argument's schema, in one document, so that *what changed for callers between two tags* is two
`curl`s and a `diff` instead of a clone at two tags. It is the machine-readable form of the answer this whole
file is about — six of the seven rows below are silent, and the release notes were the only published place
the answer existed.

**The toolkit does not read it today and nothing here assumes it will.** Two consequences worth stating
rather than leaving to be discovered:

- **It is not listed in `SHA256SUMS`**, deliberately. That file is the *binaries'* manifest and
  `ensure-jdwp.sh` matches the host's asset name against its contents, so a line for a non-binary would
  change what a consumer already reading it is reading. The surface asset is therefore **invisible** to the
  existing installer, which is the safe direction. Its integrity comes from the build attestation instead
  (REL-7, #164): `gh attestation verify tool-surface-<tag>.json --repo YgorPerez/java-debugging-mcp`.
- **Its format is versioned separately from the crate**, by `surface_version`, because the crate version
  moves for reasons that have nothing to do with the surface. The bump rule lives at the field in
  `docs/tool-surface.schema.json`, whose `$id` is the URL to pin. `kind` at the document root is the
  discriminator, so a future second asset is distinguishable without sniffing for fields.

An added tool is still the "Add a **tool**" row below — nobody finds a tool the docs do not name — but the
diff that proves it exists is now published rather than reconstructable.

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

**MCP-1 (ADR-0047) is the "Change behaviour behind an existing name" row, and it is the worst kind: the one
where the docs still look right.** Supporting MCP `2026-07-28` added no tool and renamed nothing, so every
table above is unmoved — and yet whether a caller is *pushed* a `notifications/message` when a stop point
fires now depends on which protocol revision their client speaks, because the newer one makes that
notification request-scoped and a JDWP hit belongs to no request. Nothing about the toolkit breaks: the
`initialize` handshake is still served, so a client on it behaves exactly as before, and `debug.get_last_event`
was always the record rather than the hint. What has to be said out loud is the conditional — **a skill that
tells a reader "the server will tell you when it hits" is now true for one era and false for the other**, and
`debug.get_last_event`'s own description carries the corrected version. Check any skill prose that promises a
push, not just prose that names a tool.

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

**PERF-2 (#129) is the same row again, and the same mild version — on a different tool.** A *truncated*
`debug.list_threads` reply's cost line now reads
`💸 Cost: 268 JDWP packet(s) in ~17 round trip(s), 0.42ms each …` where it used to read
`💸 Cost: 268 JDWP packet(s), 0.42ms each …`. Same shape of change as PERF-1's, same reason it is mild — a
clause added between two things already there — and the same thing to state: the per-packet figure falls for
the same work, because the names and statuses PERF-1 waved stopped being waited on one at a time.

Two differences from PERF-1's row worth naming. The clause **suppresses itself when the two numbers are
equal**, so a listing short enough to have waved nothing reads exactly as it did before and a downstream
example quoting one may or may not show the clause depending on the pool it was captured against — the clause
appearing *is* the information that something overlapped. And it is on the **truncated** reply only, because
that is the only shape that ever printed a cost line.

Nothing else in PERF-2 changed a reply. The renderer got substantially cheaper — a `Reserva` row costs
**5.36ms** of wire time where it cost 79.19ms before PERF-1, a 14.8x cut to under one round trip per row, and a
deep `debug.get_stack --expand_objects` walk sends a quarter fewer commands — and every one of those replies is
byte-identical. That is the point of it:
`debug.evaluate`, `debug.get_stack` and `debug.run_named_query` cost less and say the same thing, so the
release notes owe the speed but no caller has anything to re-read.

**DOC-9 (#132) is the one row on this table that can BREAK a caller, and the only entry here that is not
merely a documentation risk.** Every `debug.*` tool now **refuses** an argument it does not recognise, where it
used to discard it in silence. A client that sends a field this server does not have — a leftover argument, a
camelCase spelling, anything speculative — stops working, loudly, where before it was quietly ignored.

That is the change, and it was taken deliberately because the silence had a cost that outweighed it.
`resolve_session` reads `session_id` from the raw arguments of every tool, so a key it cannot find is
indistinguishable from one that was never sent: `sessionId` fell back to the **current session**, and a call
naming one JVM executed against another with a reply that looked entirely normal. `debug.attach`'s own
description is built on the difference between a JVM that is yours and one that is shared; on a mix of the two
that silence could put a suspension on somebody else's app server.

For the toolkit specifically, two things follow:

- **Its skills quote tool arguments, and any argument they name that this server does not have is now an
  error rather than a no-op.** That is worth an audit on the pin bump, and it is the good kind of break: it
  surfaces at the first call with the field named and the real alternatives listed, rather than silently doing
  the wrong thing. This repo's own suite found two such arguments the moment the check landed — both tests
  passing `on_write` to `debug.set_field_stop`, which has `modify`, dead since FILT-6 (#83).
- **`session_id` is now published in all 38 `inputSchema`s**, so `argument-schemas.txt` went from 184
  arguments to 222. It was always accepted and documented in prose in exactly two tool descriptions, which is
  not the same as being published — a client generating calls from the schema could not have known it existed.
  Nothing about what it *does* changed.

Both belong in the release notes in caller-visible terms, and the first belongs near the top: it is the only
change in this range that can make a working caller stop working.

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
it covers **42 tools and 239 arguments** by construction: each argument's full schema minus its description
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
