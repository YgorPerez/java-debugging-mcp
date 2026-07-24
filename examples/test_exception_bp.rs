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
// End-to-end test of EXC-1 exception breakpoints:
//   set_exception_request(target only) -> Exception wire-parse -> event fires on the target throw
//   with the right exception type + a catch location; the non-target exception never fires because
//   no request was set for it (selectivity by construction).
// Usage: cargo run --release --example test_exception_bp -- [port] [target-fqcn]
// Defaults: 8788, java.lang.NumberFormatException  (pair with the ExcProbe probe).

use std::time::Duration;

use jdwp_client::events::EventKind;
use jdwp_client::{JdwpConnection, SuspendPolicy};

async fn next_event(conn: &JdwpConnection, secs: u64) -> Option<jdwp_client::EventSet> {
    match tokio::time::timeout(Duration::from_secs(secs), conn.recv_event()).await {
        Ok(ev) => ev,
        Err(_) => None, // timed out
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut argv = std::env::args().skip(1);
    let port: u16 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(8788);
    let target = argv.next().unwrap_or_else(|| "java.lang.NumberFormatException".to_string());
    let target_sig = format!("L{};", target.replace('.', "/"));
    let non_target_sig = "Ljava/lang/ArithmeticException;";

    println!("Connecting to localhost:{port}...");
    let mut conn = JdwpConnection::connect("localhost", port).await?;
    println!("✓ Connected");

    // The target exception class must be loaded so we can pin the request to its ref type.
    let cls = conn.classes_by_signature(&target_sig).await?;
    let target_id = cls.first().map(|c| c.type_id)
        .ok_or_else(|| format!("{target} not loaded — the probe should pre-touch it"))?;
    println!("✓ {target} loaded (type_id 0x{target_id:x})");

    // Set an exception request on the TARGET only (caught=true, uncaught=false). We deliberately
    // do NOT request the non-target ArithmeticException — so if the filter works we never see it.
    let req = conn.set_exception_request(Some(target_id), true, false, SuspendPolicy::All).await?;
    println!("✓ Exception request set (request id {req}) — target only, caught");

    let mut target_hits = 0;
    let mut non_target_hits = 0;
    for _ in 0..6 {
        let Some(ev) = next_event(&conn, 15).await else {
            break; // no more events in the window
        };
        let Some((thread, exc, catch)) = ev.events.iter().find_map(|e| match &e.details {
            EventKind::Exception { thread, exception, catch_location, .. } =>
                Some((*thread, *exception, catch_location.clone())),
            _ => None,
        }) else {
            // Some unrelated event kind — resume and keep waiting.
            conn.resume_all().await?;
            continue;
        };

        let tref = conn.get_object_reference_type(exc).await?;
        let tsig = conn.get_signature(tref).await?;
        println!("⚡ Exception fired: type={tsig} caught={} thread=0x{thread:x}", catch.is_some());

        if tsig == target_sig {
            target_hits += 1;
            assert!(catch.is_some(), "caught=true request but no catch location on a caught throw");
        } else if tsig == non_target_sig {
            non_target_hits += 1;
        }

        conn.resume_all().await?;
        if target_hits >= 2 {
            break;
        }
    }

    conn.clear_exception_request(req).await?;
    conn.resume_all().await?;

    assert!(target_hits >= 1, "target exception ({target}) never fired");
    assert_eq!(non_target_hits, 0, "non-target ArithmeticException fired — request filter leaked");

    println!("\n🎉 EXCEPTION BREAKPOINT WORKS END TO END ({target_hits} target hit(s), {non_target_hits} non-target)");
    Ok(())
}
