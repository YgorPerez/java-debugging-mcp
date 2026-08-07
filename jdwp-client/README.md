# java-debugging-jdwp-client

A JDWP (Java Debug Wire Protocol) client — the transport and command layer behind
[`jdwp-mcp`](https://crates.io/crates/jdwp-mcp), a native Java debugger exposed as an MCP server.

It implements the subset of JDWP that practical debugging needs: connection management, breakpoint and
event-request operations, stack and variable inspection, expression evaluation, and execution control.

## What is supported

**The operations this library implements, and the types in their signatures** (ADR-0044). Everything
beneath them — the JDWP constant tables, the raw packet send, the event loop — is internal.

So: `JdwpConnection` and its operations, `JdwpError`, and the values, locations, frames, fields, methods
and events those operations return. If a `debug.*` tool in `jdwp-mcp` can do it, this crate exposes the
primitive it is built on.

**A JDWP command this crate does not implement is a pull request, not a workaround.** There is deliberately
no public way to assemble and send an arbitrary packet: that would make the whole specification
transcription part of the surface, and it would route around the read-only guard the debugger puts on every
mutating primitive.

- **This is not a compatibility guarantee.** The version check in the repository keeps the version *number*
  honest about what changed; it does not promise that nothing will.
- **Nothing is deprecated before removal**, because there is no deprecation cycle to run. **Pin an exact
  version.**
- What the surface *is* is **chosen** — so a break in it is a decision somebody made and wrote down, rather
  than a side effect of refactoring an internal.

It is the code a real debugger runs against real JVMs, tested against JDK 11, 17 and 21.

## You probably want the debugger

If you came here for Java debugging rather than for the protocol layer:

```bash
cargo install jdwp-mcp
```

Or download a prebuilt binary — no Rust toolchain needed — from the
[releases page](https://github.com/YgorPerez/java-debugging-mcp/releases/latest).

## Documentation

The narrative lives on the items themselves; see [docs.rs](https://docs.rs/java-debugging-jdwp-client).
Design decisions are in the repository's `docs/adr/`, and `CONTEXT.md` is the glossary — *stop point*,
*trace*, *snapshot*, *hit* and *suspension* all have precise meanings that are not guessable from the
type names.

## License

MIT — see [LICENSE](https://github.com/YgorPerez/java-debugging-mcp/blob/main/LICENSE).
