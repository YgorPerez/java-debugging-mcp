# A Published Documentation Site

There is no Jekyll site, no `docs/_config.yml`, no `index.md` and no Pages workflow, and this is a
decision rather than a gap. DOC-12 (#139) proposed publishing `docs/` as a site split by audience, after
comparing this repo against [`kpanuragh/xdebug-mcp`](https://github.com/kpanuragh/xdebug-mcp), which does.
It was closed as out of scope; this file is why, so the next comparison audit gets an answer instead of
re-filing it.

## The decision that settles it is the audience, and the issue said so itself

DOC-12 listed four things needing a decision before it was actionable, and its second was *"what is the
audience?"* — answering, of one branch, that **"the current shape is already correct and this issue is
mostly moot."** That is the branch. The primary reader of this project's documentation is a model reading
tool descriptions through `tools/list`, and after it a maintainer or agent working inside the repo.

That is not a hedge about who might turn up. It is what the artefacts are built for and gated on:

**The tool descriptions are the product's documentation, and they are already the long-form ones.**
ADR-0039 argues that the glossary's length *is* the evidence, and the same reasoning produced tool
descriptions that run to paragraphs. DOC-5 through DOC-9 are a run of issues about their quality. A caller
reading a schema gets the whole argument — why a filter is refused, what a measurement includes, which of
two readings a number supports — at the point of use, which is the one place a site cannot put it.

**`docs/tools.md` is gated, and a hand-written page would not be.** It is generated from committed schema
snapshots (`UPDATE_TOOL_DESCRIPTIONS=1`), which is the mechanism that has kept it honest. DOC-7 (#108) is
what an ungated documentation path produced here: interleaved gibberish, shipped in a release. A site of
narrative pages has no such gate by construction, so it would be the second ungated doc path in a repo
that has already paid for the first one.

**A published copy of the same 26 KB table is a second thing to keep in sync and fixes nothing.** DOC-12
concedes this in its own first question. The generous reading of xdebug-mcp's docs is that they are split
by audience rather than that they are hosted — and splitting by audience is a change to what is written,
which needs no host at all.

## What this is deliberately not saying

**Not that the narrative page would be worthless.** "Here is the shared-JVM workflow in five steps" is a
real missing artefact, and `README.md`'s three example prompts are not it. That page can be written
in-repo whenever someone wants it, and it needs no `_config.yml`, no Pages workflow and no second copy of
the tool table. It is a much smaller thing than this issue asked for, and rejecting the site does not
reject it.

**Not that discoverability does not matter.** It was the real problem underneath, and it was answered a
different way: REL-3 (#137) added `server.json`, the MCP registry manifest, so someone looking for this
server finds it from the registry rather than from a search engine reaching a Pages site.

**Not that `TODO.md` should be published.** DOC-12's third question asked whether 182 KB of
shipped-and-why belongs in public. It is an engineering log, it reads as one, and it is load-bearing for
agents working in the repo. Publishing it would put the least audience-shaped document here on the most
public surface, which is the reverse of what the issue wanted.

## What would change this

Not another comparison audit finding a project that has a site — that is how this arrived, and the
existence of somebody else's `_config.yml` is not an argument. Concretely:

1. **A human reader who was actually lost**, named, with what they were trying to decide. DOC-12 was
   generated during a comparison audit rather than from anyone's report, which is the same provenance
   test PROF-1 failed and TRACE-15 (#156) passed.
2. **The in-repo narrative page written first**, and found insufficient *because it was not published*
   rather than because it did not exist. That ordering costs almost nothing and settles the question with
   evidence: if the page is what was wanted, the site was never the ask.
3. **A gate for the hand-written half**, stated before it ships. DOC-7 is the recorded cost of not having
   one, and "we will keep it current" is what that path already promised once.

## Related

- ADR-0039 — the glossary is long-form on purpose, and the length is the evidence
- DOC-4 (#11) — the ADRs as the record of resolved decisions
- DOC-7 (#108) — what an ungated documentation path shipped
- REL-3 (#137) — `server.json`, which answers the discoverability half
- `.out-of-scope/profiling-and-coverage.md` — the precedent: a feature rejected because the question it
  claimed to answer was already served, and the comparison row it arrived on was not an argument
