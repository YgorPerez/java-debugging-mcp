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
#![cfg_attr(test, allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic_in_result_fn
))]
// JDWP client library for Java debugging
//
// Implements a subset of the JDWP protocol focused on practical debugging scenarios:
// - Connection management
// - Breakpoint operations
// - Stack inspection
// - Variable evaluation
// - Execution control

pub mod connection;
pub mod protocol;
pub mod commands;
pub mod events;
pub mod eventloop;
pub mod types;
pub mod reader;
pub mod vm;
pub mod reftype;
pub mod method;
pub mod eventrequest;
pub mod thread;
pub mod stackframe;
pub mod string;
pub mod object;
pub mod eval;
pub mod extra;

pub use connection::JdwpConnection;
pub use eventloop::{EventLoopHandle, spawn_event_loop};
pub use events::EventSet;
pub use protocol::{JdwpError, JdwpResult};
pub use eventrequest::SuspendPolicy;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
