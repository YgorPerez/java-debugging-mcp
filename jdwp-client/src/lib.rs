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
//! # This is not a supported public API
//!
//! **The crate is published because `jdwp-mcp` depends on it, and for no other reason.** `cargo publish`
//! rejects a bare path dependency, so a binary on crates.io requires its library to be there too; that
//! requirement is the whole story of this listing.
//!
//! What follows from that, and it is worth reading before you build on this:
//!
//! - **The surface is shaped for one consumer.** These modules are the seams `jdwp-mcp` needed, exposed
//!   where it needed them. They are not a curated library API, and several are public only because a
//!   sibling module had to reach them.
//! - **Anything here may change in any release**, including a patch one. The version gate in this
//!   repository exists to keep the *version number* honest about what changed, not to promise that
//!   nothing will.
//! - **Nothing here is deprecated before it is removed**, because there is no deprecation cycle to run.
//!
//! None of that means it will not work — it is the code a real debugger runs against real JVMs, and it
//! is tested against JDK 11, 17 and 21. It means the cost of a break lands on you rather than on us, and
//! that pinning an exact version is the only safe way to depend on it.
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
pub use eventloop::{spawn_event_loop, EventLoopHandle};
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
