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
// JDWP client library for Java debugging
//
// Implements a subset of the JDWP protocol focused on practical debugging scenarios:
// - Connection management
// - Breakpoint operations
// - Stack inspection
// - Variable evaluation
// - Execution control

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

pub use connection::JdwpConnection;
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
