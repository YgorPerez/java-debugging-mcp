# java-debugging-jdwp-client

A JDWP (Java Debug Wire Protocol) client — the transport and command layer behind
[`jdwp-mcp`](https://crates.io/crates/jdwp-mcp), a native Java debugger exposed as an MCP server.

It implements the subset of JDWP that practical debugging needs: connection management, breakpoint and
event-request operations, stack and variable inspection, expression evaluation, and execution control.

## This is not a supported public API

**It is published because `jdwp-mcp` depends on it, and for no other reason.** `cargo publish` rejects a
bare path dependency, so a binary on crates.io requires its library to be there too. That requirement is
the whole story of this listing.

- **The surface is shaped for one consumer** — these modules are the seams `jdwp-mcp` needed, exposed
  where it needed them, not a curated library API.
- **Anything may change in any release**, including a patch one. The version check in the repository
  keeps the version *number* honest about what changed; it does not promise that nothing will.
- **Nothing is deprecated before removal**, because there is no deprecation cycle to run.

None of that means it does not work — it is the code a real debugger runs against real JVMs, tested
against JDK 11, 17 and 21. It means the cost of a break lands on you, and that pinning an exact version
is the only safe way to depend on it.

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
