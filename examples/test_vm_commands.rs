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
// Test VirtualMachine commands (Version)
//
// This checked `IDSizes` too, until CLEAN-1 (#27) deleted that wrapper: the id widths are assumed
// 8-byte by the reader and never consulted (see the header of `jdwp-client/src/reader.rs`). This
// harness was its only caller — which is not a use, since nothing runs it in the suite; that is why the
// coverage run measured the command at zero hits.

use jdwp_client::JdwpConnection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Enable tracing
    tracing_subscriber::fmt()
        .with_env_filter("jdwp_client=debug")
        .init();

    println!("Connecting to JDWP at localhost:5005...");
    let mut connection = JdwpConnection::connect("localhost", 5005).await?;
    println!("✓ Connected\n");

    // Get version info
    println!("Fetching VM version...");
    let version = connection.get_version().await?;
    println!("✓ Version received:");
    println!("  Description: {}", version.description);
    println!("  JDWP: {}.{}", version.jdwp_major, version.jdwp_minor);
    println!("  VM Version: {}", version.vm_version);
    println!("  VM Name: {}", version.vm_name);

    println!("\n🎉 VM version command working!");

    Ok(())
}
