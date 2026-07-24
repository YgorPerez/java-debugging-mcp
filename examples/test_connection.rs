// Throwaway JDWP protocol test harness (manual, ad-hoc) — not production code;
// stdout / `unwrap` / indexing / panics are fine here.
#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic_in_result_fn,
    clippy::manual_unwrap_or_default
)]
// Simple test to verify JDWP connection works

use jdwp_client::JdwpConnection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Enable tracing
    tracing_subscriber::fmt()
        .with_env_filter("jdwp_client=debug")
        .init();

    println!("Connecting to JDWP at localhost:5005...");

    let connection = JdwpConnection::connect("localhost", 5005).await?;

    println!("✓ Successfully connected and completed handshake!");
    println!("Connection: {:?}", connection);

    Ok(())
}
