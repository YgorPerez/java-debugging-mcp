// Lint policy — mirror the rust-doctor health gate (see `.github/workflows/`)
// locally so `cargo clippy` surfaces exactly what CI does. rust-doctor enables
// clippy's pedantic/nursery/cargo groups plus a curated set of restriction
// lints via command-line flags; declaring them here keeps the two in sync.
#![warn(clippy::pedantic, clippy::nursery)]
#![warn(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::format_push_string,
    clippy::panic_in_result_fn
)]
// Restriction lints above target production code; unit tests may panic on failure, so `unwrap`,
// `expect`, indexing, and assertions are idiomatic there.
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic_in_result_fn)
)]
//! A JDWP (Java Debug Wire Protocol) client — the transport and command layer behind the `jdwp-mcp`
//! debugging server.
//!
//! Implements the subset of JDWP that practical debugging needs: connection management, breakpoint and
//! event-request operations, stack and variable inspection, expression evaluation, and execution control.
//!
//! # What is supported
//!
//! **The operations this library implements, and the types in their signatures.** That is the whole of it,
//! and everything beneath them — the JDWP constant tables, the raw send, the event loop — is `pub(crate)`
//! (ADR-0044).
//!
//! So: [`JdwpConnection`] and its operations; [`JdwpError`] and `JdwpResult`; the values, locations,
//! frames, fields, methods and events those operations return. If a `debug.*` tool in `jdwp-mcp` can do it,
//! this crate exposes the primitive it is built on.
//!
//! **A JDWP command this crate does not implement is a pull request, not a workaround.** There is
//! deliberately no public way to assemble and send an arbitrary `CommandPacket` — that would make the
//! entire specification transcription part of the surface, and it would route around the read-only guard
//! ADR-0001 puts on every mutating primitive.
//!
//! ## What that promise is, and is not
//!
//! It is **not** a compatibility guarantee. The version gate in this repository keeps the version *number*
//! honest about what changed; it does not promise that nothing will. There is no deprecation cycle, so
//! nothing is deprecated before it is removed, and **pinning an exact version is still the right way to
//! depend on this**.
//!
//! What it is: a statement that the surface above was *chosen*, so a break in it is a decision somebody
//! made and wrote down rather than a side effect of refactoring an internal. Before ADR-0044 this crate
//! declined to say even that — 169 public items existed that nothing outside it used, 130 of them a
//! transcription of the JDWP specification, and no way to tell design from residue.
//!
//! It is the code a real debugger runs against real JVMs, tested against JDK 11, 17 and 21.
//!
//! If you want the debugger rather than the protocol layer, install `jdwp-mcp`.
//!
//! # Where the documentation is
//!
//! The narrative lives on the items themselves rather than here. The design decisions behind them are in
//! the repository's `docs/adr/`, and `CONTEXT.md` is the glossary for the vocabulary these types use —
//! *stop point*, *trace*, *snapshot*, *hit* and *suspension* all have precise meanings that are not
//! guessable from the type names.

pub mod commands;
pub mod connection;
pub mod eval;
pub mod eventloop;
pub mod eventrequest;
pub mod events;
pub mod extra;
pub mod method;
pub mod object;
pub mod protocol;
pub mod reader;
pub mod reftype;
pub mod stackframe;
pub mod string;
pub mod thread;
pub mod types;
pub mod vm;

pub use connection::{JdwpConnection, MAX_READS_IN_FLIGHT};
#[allow(unused_imports)]
// re-exported for the crate root's own convenience; ADR-0044 keeps both internal
pub(crate) use eventloop::{spawn_event_loop, EventLoopHandle};
pub use eventrequest::{EventFilters, MonitorKind, SuspendPolicy, WatchKind};
pub use events::EventSet;
pub use protocol::{JdwpError, JdwpResult};

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
