# 0047 — Two eras are served, and an alert has nowhere legal to go in the newer one

**Status:** Accepted
**Date:** 2026-08-11
**Issue:** MCP-1 ([#180](https://github.com/YgorPerez/java-debugging-mcp/issues/180))

## Context

This server declared MCP `2024-11-05` and answered three methods: `initialize`, `tools/list`,
`tools/call`. Revision [`2026-07-28`](https://modelcontextprotocol.io/specification/2026-07-28) makes the
protocol **stateless** — it removes the `initialize`/`notifications/initialized` handshake outright, and
every request instead carries its own protocol version and client capabilities in `_meta`. It also adds a
`server/discover` RPC that a server **MUST** implement, requires a `resultType` on every result, and
requires `ttlMs`/`cacheScope` on list replies.

Most of the revision does not reach this server, and it is worth saying why so nobody re-audits it: the
large additions are Streamable-HTTP concerns — authorization, `Mcp-Method`/`Mcp-Name` headers,
`x-mcp-header`, SSE resumability, the removal of `Mcp-Session-Id` — and this server is stdio-only
(`.out-of-scope/http-transport.md`). Multi Round-Trip Requests replace server-initiated requests, and this
server initiates none. `subscriptions/listen` replaces `resources/subscribe`, and there are no resources.

Two things did reach it, and they pull in opposite directions.

**Every client that works today speaks the older contract.** The pinned downstream toolkit installs a
release and documents its tools in skills; Claude Code runs it as a plugin. Six of the seven ways a change
here reaches that toolkit are silent (`docs/toolkit-contract.md`), so a revision bump that stopped
answering `initialize` would look exactly like success from inside this repo.

**The newer revision has no channel for an unsolicited message.** EVT-2's whole point is that a suspending
stop point pushes `notifications/message` the moment it fires, because the caller is not asking at that
instant — the debuggee's clock produced the event. Under `2026-07-28`:

- `notifications/message` is **request-scoped**, and a server **MUST NOT** emit one for a request that did
  not carry `io.modelcontextprotocol/logLevel`. A hit belongs to no request at all.
- `subscriptions/listen` cannot carry it either: its opt-in types are a closed set about list changes and
  resource subscriptions.
- Logging is **deprecated** in the same revision, with stderr named as the migration for stdio.

So the push is not merely discouraged; there is nowhere to put it.

## Decision

### Dual-era, and the client's opening message selects which

A request carrying `_meta['io.modelcontextprotocol/protocolVersion']` is served statelessly per
`2026-07-28`. An `initialize` request selects the legacy contract and still negotiates `2024-11-05`,
unchanged. This is the spec's own dual-era model, and it is the only row of its compatibility matrix where
a legacy client keeps working.

**A request with no `_meta` is a legacy request, not a malformed one.** This is the load-bearing choice.
The spec says a modern request missing a required field **MUST** be rejected `-32602` — but a request with
no `_meta` at all is not a modern request; it is indistinguishable from what every client sent before this
revision existed, and `stdio_protocol.rs` has been sending exactly that since TEST-9. Rejecting it would
satisfy a rule about modern requests by breaking the compatibility the same document requires.

`server/discover` is the exception, and the only one: it exists solely in the modern era, so a missing
version there has no other reading and is `-32602`.

### `server/discover` answers, and answering is the contract

On stdio a dual-era *client* probes with `server/discover` and falls back to `initialize` on **any**
non-modern error or timeout. So the significant property is not the payload but that a probe is answered
with a `DiscoverResult` rather than an error — an error of any kind, however well-meant, tells the client
this server is legacy. `the_discovery_probe_answers_with_a_result_and_not_an_error` asserts that shape
first and the fields second.

`supportedVersions` lists the modern versions only. `2024-11-05` is deliberately absent: it is reachable
through a handshake, not selectable by a `_meta` field, and listing it would invite a combination that
does not exist.

### An alert goes to stderr for a modern client, and the buffer is the answer

For a legacy client, nothing changes: `notifications/message`, gated on the handshake, exactly as before.
For a modern client the same text is written to **stderr**, which the stdio binding blesses for exactly
this, and `debug.get_last_event` is the supported way to learn a hit happened.

**This cost nothing to the design, and that is a fact about EVT-2 rather than about this ADR.** The
glossary has said since it was written that an alert is *best-effort, and everything one carries is also
readable by asking, so nothing depends on one arriving*. Because the buffer was always the record and the
push always a hint, a revision that removed the channel removed a transport rather than a feature. Had
anything been built to depend on the push, this would have been a capability loss.

`server/discover` therefore does **not** declare the `logging` capability, while `initialize` still does.
The capability describes what this server will do for *this* peer, and the answer genuinely differs;
declaring notifications that can never arrive would be the silent promise this codebase exists to refuse.

### Every result is stamped in one place

`resultType: "complete"` and `_meta['io.modelcontextprotocol/serverInfo']` are added centrally in
`handle_request`, not per handler. `resultType` is required on every result, a reply without one is
invalid to a conforming client, and `tools/call` alone has several return paths — a per-handler convention
would be one of them away from a protocol violation.

Legacy replies are stamped too. A result is an open object in every revision, so both keys are inert to a
client that predates them, and clients on earlier revisions are instructed to read an *absent* `resultType`
as `"complete"` — never to reject a present one. One path cannot drift from another the way two would.

## Rejected alternatives

**Modern-only.** Cleanest code, and it breaks every current consumer, including a pinned release this
repo's own contract document says cannot see the break coming.

**Keep pushing to modern clients anyway.** Preserves EVT-2 everywhere at the cost of being knowingly
non-conformant on the one revision being added. Rejected: the reason to implement a spec is that a client
can rely on it.

**Buffer alerts and flush them onto the next request that sets `logLevel`.** Strictly legal. Rejected
because a JDWP hit almost never coincides with an in-flight request that opted into logging, so in practice
this is stderr with a queue in front of it — and it would mean adopting a feature deprecated in the same
revision.

**Track the era per request instead of per process.** The stateless model's whole point is that no state
spans requests, so this looks like the conformant choice. It is not available: an alert fires when no
request is in flight, so there would be nothing to read the era *from*. The era is recorded as a property
of the peer, nothing about a reply is derived from it, and the spec's own compatibility matrix tells a
dual-era server to infer it from how the client opens.

**Sort `tools/list` to satisfy the new deterministic-order rule.** Unnecessary: `get_tools()` builds a
fixed vector, so the order is already stable across requests. Sorting would change the order every current
consumer sees in exchange for nothing.

## Consequences

- 42 tools, unchanged. No tool was added, removed or renamed, and `tools/list` returns byte-identical
  entries in both eras — asserted, because "the tool set MUST NOT vary per connection" is now a rule.
- `debug.get_last_event`'s description says polling is not optional for a modern client. That is a
  behaviour change reaching the downstream toolkit through the one route that is not silent, so it belongs
  in the release notes as well.
- The `Era` enum lives beside the `Alerter` because that is the only thing that consults it.
- Two of the three MCP-defined error codes are deliberately not implemented: `HeaderMismatch` is HTTP-only,
  and `MissingRequiredClientCapability` cannot arise because no tool here needs a client capability —
  nothing samples, elicits or reads roots. A code that can never be emitted is a claim about behaviour
  that does not exist.
- `ttlMs` is an hour. The tool set is compiled in and cannot change while the process lives, so the figure
  bounds staleness after an *upgrade*, not after a change.
- Nothing in `jdwp-client` changed, and no JDWP command was added: this revision is entirely about the MCP
  side of the process (SAFE-12's table is untouched).
