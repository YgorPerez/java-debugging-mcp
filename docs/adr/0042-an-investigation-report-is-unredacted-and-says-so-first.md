# 0042 — An investigation report is unredacted, and says so first

## Context

TRACE-14 ([#136](https://github.com/YgorPerez/java-debugging-mcp/issues/136)) asked for a way to emit an
investigation, found by comparing this server against
[`kpanuragh/xdebug-mcp`](https://github.com/kpanuragh/xdebug-mcp)'s `export_session` (JSON or HTML). Today an
investigation is readable exactly once, by exactly one reader: the model that called `debug.get_traces`. The
trace buffer is a bounded ring, so the early hits of a long trace are gone by the time the interesting one
arrives, and everything else the session learned — which stop points were armed and where, their measured
capture costs, the caller chains, the staleness verdicts — exists only as text already spent in a context
window that will be summarised or discarded.

That matters more here than on a per-request PHP debugger because the sessions are longer and **the findings
are the deliverable**. The evidence a shared-JVM diagnosis rests on is frequently "here are 40 snapshots
showing the tenant arriving null on 11 of them", which is a thing to attach to a ticket rather than paraphrase.

## Decision

**`debug.export_investigation` emits the session as one Markdown report, unredacted, opening with a paragraph
that says so.**

### No redaction, and the sentence is what replaces it

Snapshots hold whatever the debuggee's variables held: request and response payloads, bearer tokens in a header
or a field, credentials sitting in a `byte[]`, customer records. None of it is altered.

**Pattern-based redaction is rejected**, and not on cost. A redactor that misses one secret is *worse* than no
redactor, because its output implies the file was cleaned — the same inversion this project files most of its
issues about, a mechanism whose result reads as a stronger guarantee than it gives. Nothing here implies that.

This is the posture everywhere else too: ADR-0023's heap query ships with the pause it imposed printed in its
own reply, ADR-0010's traced stop point reports its own measured cost, TRACE-15
([#156](https://github.com/YgorPerez/java-debugging-mcp/issues/156)) added a count rather than a refusal.
Report the cost; never silently alter the answer.

Three things make the warning act-on-able rather than decorative, and each is asserted by a test:

- **It names what to look for** — payloads, tokens, `byte[]` credentials — rather than saying "may contain
  sensitive data". A caller deciding whether to attach a file needs to know what to grep for, and a generic
  warning is one nobody acts on.
- **It carries why there is no redactor**, in the reply and not only in this ADR. Otherwise the next person
  reads the absence as an unfinished feature and adds one.
- **It can never imply the opposite.** The test forbids "sanitised", "has been cleaned", "safe to share" and
  their neighbours outright. That assertion is the one with teeth.

**It is first, before any content**, because a reader who has scrolled past the warning has already read the
payloads it warns about.

### The session, not the buffer — so a tool, not a flag

Under **ADR-0015**'s rule, a flag may change how an answer is rendered but not what the question was. The
report covers the attach target, the VM version, every stop point with its measured cost, the drift verdicts
and the disarms; none of that is in the trace buffer, so `debug.get_traces {export: true}` would have changed
the question. It is a tool of its own.

The stop-point section is **the same renderer `list_stop_points` uses**, so the report cannot drift from the
listing — and it brings the staleness verdict with it, which answers #136's third question without a second
mechanism.

### Markdown

The consumer is a model or a ticket and both prefer it. JSON's one advantage is diffing two sessions, which
nobody has asked for, and a JSON dump would lose the prose that makes a snapshot interpretable — the caller
chain, the measured cost, the verdict. HTML is what the upstream comparison produces and is the wrong default
for either consumer here.

### It never clears, and cannot preserve a trace beyond the ring

`debug.get_traces` already has an explicit `clear`; an export that silently emptied the buffer would be
destructive by default, and a single-call test cannot tell "read the buffer" from "drained the buffer" — so the
test calls it twice.

Draining-as-it-fills, which #136 raises as the only thing that would truly preserve a long trace, is **not
possible without the write path ADR-0041 declined**. So it is not offered. What the report does instead is
*state the loss*: `trace_seq` counts every record ever filed and `traces.len()` is what survives, so the
difference is exactly what a reader cannot see. TRACE-9 ([#80](https://github.com/YgorPerez/java-debugging-mcp/issues/80))
established that a capture truncates at capture time and the cut is irreversible; the ring adds a second,
coarser loss on top. A partial record that does not admit it is a misleading one.

## Rejected alternatives

- **Pattern-based redaction** — above. The decisive argument is that it implies a cleaning it cannot deliver.
- **A flag on `debug.get_traces`** — above, under ADR-0015.
- **JSON or HTML** — above.
- **Draining as it fills** — above; it needs the write path ADR-0041 rejected on the safety model.
- **Filtering arguments** (`bp_id`, `class_filter`, `since`) mirroring `get_traces`. A report is the whole
  investigation; a filtered one is what `get_traces` is already for, and offering both here would invite a
  report that quietly omits the evidence that mattered.

## Consequences

The tool count goes 39 → 40 and `CONTEXT.md` gains **Investigation report**.

`export` now names two artefacts — the **stop-point set** (ADR-0041) and the **investigation report** — and
means the same thing for both: *emit in a form that outlives the session*. The artefacts carry the distinct
names. This was checked deliberately before shipping rather than after, because one word doing two unrelated
jobs is what `inherited` did for a day in ADR-0040.

`get_version` was already reached — `can_get_method_return_values` calls it to decide between `METHOD_EXIT`
kinds 41 and 42, so every `debug.set_method_exit_stop` has been going through it. (An earlier draft of this ADR
called it dead code and the report its first caller. That was wrong, and the coverage review salvaged out of
`TODO.md` is what caught it: it had recorded `get_version` as "now reached, 2 hits, via attach" and was right.)
What the report adds is the first use of the *result* as an answer rather than as a version gate. The one round
trip it costs is worth it because "which JVM was this?" is the first question asked of an attached report and
`endpoint` does not answer it across a redeploy. A failure to read it is **stated** rather than omitted, since
an absent line reads as "no VM information exists" when what happened is that one command failed.
