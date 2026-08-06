# 0041 — The server returns the set and the client stores it

## Context

BP-8 ([#135](https://github.com/YgorPerez/java-debugging-mcp/issues/135)) asked for stop points that outlive
the process, found by comparing this server against
[`kpanuragh/xdebug-mcp`](https://github.com/kpanuragh/xdebug-mcp)'s `save_debug_profile` /
`load_debug_profile` / `list_debug_profiles`. Eight stop points across four classes, with their conditions,
thread filters, `trace_expr` lists and `hit_count`s, are gone at `debug.disconnect` — and under stdio the
client's lifetime *is* the session's, so closing the client is the same thing. Re-entering the same
investigation tomorrow means re-arming by hand, and the details easiest to get wrong (which line inside the
`finally`, which classloader, which `hit_count`) are the ones written down nowhere.

The issue named three places it could live: a file under the project, a dotfile in `$HOME`, or content the
client stores and passes back.

## Decision

**The server returns the set as content; the client stores it.** `debug.list_stop_points {export: true}`
emits a **stop-point set**, and the new `debug.arm_stop_points {set: …}` takes it back. Nothing here touches
the filesystem.

**A file under the project and a dotfile in `$HOME` are both rejected**, and the reason is the safety model
rather than tidiness. Everything this server promises about a shared JVM leans on process death being an
**unambiguous end of session** — the watchdog, the resume accounting, the read-only enforcement. State that
outlives the process on disk is state no live process can vouch for. It would also arrive with a policy about
where output lands, which this project has so far not had to have, and a write path it has so far not had at
all: the only `fs::write` calls in the crate are test snapshots.

### The format is the list of calls that would recreate the set

An entry is `{tool, enabled, args}` — literally a `debug.set_*` call. `debug.arm_stop_points` replays each one
through **the same handler a caller would reach**, so every refusal, clamp, capability check, deferral and
read-only rule applies on the way back in without being reimplemented.

This is the load-bearing choice, not an encoding convenience. A parallel arming path would be a second place
for those rules to live and a second place for them to drift — and the rules it would drift on are the ones
that keep a shared JVM alive. It also means the format is checked by something that already exists: the
argument schemas are snapshot-tested, so an argument renamed or dropped cannot silently invalidate every saved
set.

**`ARMABLE_TOOLS` is a whitelist and it routes.** Only the five arming tools may appear in a set. Without it
this tool would be a way to invoke anything in this server from a blob of JSON, which is a different and much
larger thing than resuming an investigation. It also *is* the dispatch predicate, so the list and the set of
handlers reachable through it cannot quietly disagree.

### A flag for the export, a tool for the arming

Under **ADR-0015**'s rule — *a flag may change how an answer is bounded, filtered or rendered; it may not
change what the question was* — `export` is a flag. "What stop points are armed?" is the same question in both
forms; one answers it for a reader and the other in a shape that can be handed back. Arming a set is a
different question, so it is a tool with its own name.

### Three things a set deliberately does not carry

**Instance filters and thread filters are dropped, and named in the reply.** An instance handle is a weak
reference to one object in one JVM (ADR-0022); a JDWP thread id is the same, and a pool that retires idle
workers invalidates it even inside one process. #135's body named only the first. `list_stop_points` already
warns about the two separately (FILT-2, FILT-9) because the cause and the fix differ, and the export follows
that split. A dropped filter leaves the entry **broader** than the one exported, which on a shared instance is
the difference between a diagnostic and an outage — so the reply says so, per stop point, above the block
rather than below it, where a reader has already started copying.

**Resolved JDWP ids are not in the format at all**, and the subtler half of this is that
`BreakpointInfo::method` is *also* a resolved value. It is filled in from whichever method the line landed in,
so it is `Some` on every armed breakpoint whether the caller named one or not. The first version of this
export wrote it into the args, turning `{line: 28}` into `{line: 28, method: "classify"}` — harmless on the
build it came from and wrong on a redeployed one, where line 28 may sit in another method and the entry then
resolves elsewhere or is refused. The same mistake as carrying an instance handle across a JVM, one level less
obvious, and it was caught only by reading a real exported block. `BreakpointInfo` therefore keeps `arm_line`
and `arm_method` — **the caller's own words** — and the set replays those.

**A disabled or spent stop point is exported and not armed.** Arming-then-disabling was considered and
rejected: it arms the stop point for real in between, which on a suspending one is long enough to fire. Its
arguments are still in the set, and the skip is reported per entry.

### Every outcome is reported, per entry

`2 armed, 1 deferred, 1 refused` leads, and every entry that is not plainly armed is then named with its
reason. DISC-14 ([#130](https://github.com/YgorPerez/java-debugging-mcp/issues/130)) established that silence
on this surface must mean *checked* and never *nobody looked*; an aggregate reading `4 armed` while one of the
four was refused is that same defect wearing a total. Nothing aborts on a bad entry, following the
wildcard/list precedent: one refused location is a normal batch result.

Deferral is read from the session (did `pending_breakpoints` grow?) rather than sniffed out of the reply text.
A substring check for "deferred" would be exactly the reply-wording dependency TEST-46
([#154](https://github.com/YgorPerez/java-debugging-mcp/issues/154)) exists to stop, and it would silently
start reporting every entry as armed the day that word changed.

**Lines are not checked against bytecode, and the reply says so whenever anything armed.** A set carries line
numbers, which are a claim about a build. `debug.check_stale` is what can settle it, and it stays a separate
call: thirty entries would be thirty round trips, and a set re-armed ten seconds later against the same JVM
needs none of them. Stated even so, because the reading to prevent is "it armed, so the lines must still be
right".

## Rejected alternatives

- **A file under the project**, and **a dotfile in `$HOME`** — above.
- **A parallel arming path** instead of replaying the handlers — above.
- **Reproducing the disabled state by arming then toggling off** — above.
- **`load_debug_profile`-style named profiles**, which is what the upstream comparison offered. Naming and
  listing profiles is storage, and storage is the thing being declined; the client already has names for
  things it saves.
- **Running `check_stale` per entry.** Rejected on cost, with the disclaimer stated instead. If it were free
  it would be right.

## Consequences

`read_only` needs nothing here, which answers #135's fifth open question: arming is not a write to the
debuggee, and an *invoking* `condition` or `trace_expr` is refused by the handler that receives it, exactly as
if the caller had typed the call out. The existing check is already in the right place.

A set is the one call that can arm **many** suspending stop points at once, where every other path arms one
and the caller reads a reply between each. That is reported — at export time and again at arm time — and not
refused, following this project's posture of reporting a cost rather than forbidding it.

`export` now means one thing on this surface: *emit in a form that outlives the session*. It is deliberately
the same word ADR-0042 uses for an investigation report, because the **artefacts** differ (a stop-point set
against a report) and those carry the distinct names. This is the check `inherited` failed in ADR-0040 — one
word, two unrelated jobs — applied before shipping rather than after.

The tool count goes 38 → 39, and `CONTEXT.md` gains **Stop-point set**.
