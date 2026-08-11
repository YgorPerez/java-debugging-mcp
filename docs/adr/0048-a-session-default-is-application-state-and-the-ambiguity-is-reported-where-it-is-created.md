# 0048 — A session default is application state, and the ambiguity is reported where it is created

**Status:** Accepted
**Date:** 2026-08-11
**Issue:** MCP-1 ([#180](https://github.com/YgorPerez/java-debugging-mcp/issues/180)), found after the fact

## Context

[ADR-0047](0047-two-eras-are-served-and-an-alert-has-nowhere-legal-to-go-in-the-newer-one.md) made this
server serve MCP `2026-07-28` statelessly and reasoned carefully about **protocol** state: the version,
the capabilities, the client's identity, and which era a peer opened in. It did not notice that the
**application** layer has the same shape, and it should have.

Two things here are established by one request and consulted by a later one:

- **The current session.** An omitted `session_id` resolves against it (`SessionManager::current_session`),
  and it is set by whichever `debug.attach` or `debug.launch` happened last, or explicitly by
  `debug.set_current_session` (SESS-1).
- **Session defaults**: `trace_expr`, the step filter, `source_roots`/`class_roots` — set once at attach and
  used by later calls that name none of their own ([ADR-0040](0040-a-session-default-not-a-second-tool-family-and-never-a-merge.md)).

The revision's Statelessness section reads:

> A server processes each request independently; no state should be inferred from previous requests, even
> those on the same connection or stream. […] State that needs to span multiple requests (e.g., long-running
> tasks, **application-level handles**) **MUST** be referenced by an explicit identifier the client passes on
> each request.

On its face that describes exactly what the two bullets above do.

**And the hazard is concrete rather than theoretical, which is what makes this worth a decision.** The same
revision permits a client to interleave unrelated work on one stdio process — *an open connection is not a
conversation or session*. So conversation A attaches to JVM 1, conversation B attaches to JVM 2 (making it
current), and A then calls `debug.force_return` with no `session_id`. It lands on **B's JVM**. One of them
may be the shared 8180, and `force_return`, `resume_thread` and `set_value` are writes.

## Decision

### The rule governs protocol context, not tool arguments

Session defaults stay, and `session_id` stays optional.

The reading this rests on is the specification's own, in the tools page's *Stateful Tools* section:

> MCP has no protocol-level session, so a server cannot rely on implicit per-connection state to relate one
> tool call to the next. Servers that need to maintain state across calls […] should do so by returning an
> explicit handle from a creation tool and accepting that handle as an argument on subsequent calls.

That is precisely what `session_id` is: minted by `debug.attach`, returned in its reply, accepted by every
tool. The same section adds that *the protocol has no concept of a state handle; from the wire's perspective
a handle is an ordinary string in a tool result and an ordinary argument to subsequent tool calls* — so the
statelessness requirement binds the protocol's own context (version, capabilities, identity, and the sessions
the revision abolished), and the ergonomics of a tool's arguments are the application's business.

**The opposite reading is available and is not silly**: the MUST names "application-level handles" by name,
and a default that lets a client omit the identifier does infer context from an earlier request. It is
recorded here so that anyone re-deriving it finds a decision rather than an oversight.

### The ambiguity is reported where it is created

A default that is *usually* right and *occasionally* reaches the wrong JVM is exactly the shape this
codebase reports rather than hides. So `debug.attach` and `debug.launch` say so at the moment a **second**
session becomes live: how many are live, that an omitted `session_id` now resolves against the one just
made current, and that `force_return`/`resume_thread`/`set_value` are where getting it wrong is a write.

Three properties of that placement, each chosen over an alternative:

- **At creation, not on every reply.** The second attach is the moment the default stops being obviously
  right and the moment a caller can act. Repeating the warning on every later reply is noise a reader learns
  to skip, which is the opposite of reporting.
- **One site, not thirty-nine.** `resolve_session` has 39 callers. Threading a note through all of them, or
  parking one on the handler for the duration of a call, would buy the same sentence for far more surface —
  and the parked-flag version would silently depend on the message loop staying sequential, which ADR-0047
  refuses to promise forever.
- **Silent while one session is live**, which is the overwhelming majority of use and unambiguous by
  construction.

## Rejected alternatives

**Require `session_id` under the modern era.** Conformant to the letter and it kills the hazard outright.
Rejected because it makes a tool's semantics depend on which era the caller opened in — the split ADR-0047
went to some trouble to avoid, and one that would put ADR-0040's defaults in the same position.

**Refuse an omitted `session_id` once a second session is live.** Genuinely attractive: conformant where it
matters, no era split, and the refusal is the diagnostic. Rejected as the *first* move because it breaks a
working call for a caller who has two sessions open and knows perfectly well which one they mean, and
because reporting has to be tried before refusing in a codebase whose posture is that a tool says what it
did. **This is the escalation if the report proves insufficient**, and it is the thing to reach for rather
than re-arguing the whole ADR.

**Say nothing and record nothing.** The cheapest option and the one this repo has the most scars from: an
unrecorded premise gets rediscovered as if it were new. `CONTEXT.md`'s **Session** entry exists for the same
reason.

## Consequences

- `debug.attach` and `debug.launch` gain a conditional line, so both tool descriptions change — a
  caller-visible change, and one for the release notes.
- **`CONTEXT.md` now defines `session`.** It had no entry while carrying three meanings, one of which
  (`Mcp-Session-Id`) the revision deleted; a reader conflating them would conclude this server leans on
  connection state and is non-conformant, which is backwards.
- `.out-of-scope/http-transport.md` carries a dated correction. Its *decisive* argument was that client
  lifetime is session lifetime, and the revision withdraws even the convention that made that true on stdio
  — so what actually differs over HTTP is **concurrency**, not lifetime, which is what its reopening bar
  already said. The verdict is unchanged.
- If concurrent request dispatch is ever adopted (ADR-0047 refuses it, and names ADR-0003 and ADR-0009 as
  why), re-read this: the interleaving hazard stops being about two conversations and becomes about two
  in-flight calls, and reporting would no longer be enough.
