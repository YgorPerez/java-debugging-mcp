// End-to-end test of the deferred (class-prepare) breakpoint machinery:
//   set_class_prepare -> ClassPrepare parse -> arm real breakpoint on the just-loaded type ->
//   resume_thread -> breakpoint actually fires.
// Usage: cargo run --release --example test_deferred_bp -- [port] [SimpleClassName] [method]
// Defaults: 8799 DeferTarget hit  (pair with the MainDefer probe).

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
    let port: u16 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(8799);
    let class = argv.next().unwrap_or_else(|| "DeferTarget".to_string());
    let method = argv.next().unwrap_or_else(|| "hit".to_string());

    println!("Connecting to localhost:{port}...");
    let mut conn = JdwpConnection::connect("localhost", port).await?;
    println!("✓ Connected");

    // Sanity: the class must NOT be loaded yet, or this isn't testing the deferred path.
    let sig = format!("L{};", class);
    let already = conn.classes_by_signature(&sig).await?;
    println!("classes_by_signature({sig}) before load -> {} match(es) (expect 0)", already.len());

    // 1) Register the CLASS_PREPARE watch (EventThread suspend, like the deferred bp path does).
    let cp_req = conn.set_class_prepare(&class, SuspendPolicy::EventThread).await?;
    println!("✓ CLASS_PREPARE watch registered (request id {cp_req})");

    // 2) Wait for the ClassPrepare event and confirm the new wire-parse produced real fields.
    let (cp_thread, cp_ref, cp_sig) = loop {
        let Some(ev) = next_event(&conn, 15).await else {
            return Err("timed out waiting for ClassPrepare (did the probe load the class?)".into());
        };
        if let Some(hit) = ev.events.iter().find_map(|e| match &e.details {
            EventKind::ClassPrepare { thread, ref_type, signature, status } if signature.contains(&class) =>
                Some((*thread, *ref_type, signature.clone(), *status)),
            _ => None,
        }) {
            println!("✓ ClassPrepare parsed: thread=0x{:x} ref_type=0x{:x} sig={} status={}", hit.0, hit.1, hit.2, hit.3);
            break (hit.0, hit.1, hit.2);
        }
    };
    assert_eq!(cp_sig, sig, "signature mismatch");

    // 3) Arm the real breakpoint on the now-loaded type (use the ref_type straight from the event).
    let methods = conn.get_methods(cp_ref).await?;
    let m = methods.iter().find(|m| m.name == method).ok_or("method not found on loaded class")?;
    let lt = conn.get_line_table(cp_ref, m.method_id).await?;
    let entry = lt.lines.iter().min_by_key(|e| e.line_code_index).ok_or("no line table")?;
    let bp_req = conn.set_breakpoint_ex(cp_ref, m.method_id, entry.line_code_index, SuspendPolicy::All, None, None).await?;
    println!("✓ Armed breakpoint at {class}.{method}:{} (request id {bp_req})", entry.line_number);

    // 4) Release the class-prepare-suspended thread so class init + the loop proceed.
    conn.clear_class_prepare(cp_req).await?;
    conn.resume_thread(cp_thread).await?;
    println!("✓ Cleared watch + resumed preparing thread");

    // 5) Confirm the breakpoint actually fires.
    let fired = loop {
        let Some(ev) = next_event(&conn, 15).await else {
            return Err("timed out waiting for the armed breakpoint to fire".into());
        };
        if let Some(loc) = ev.events.iter().find_map(|e| match &e.details {
            EventKind::Breakpoint { thread, location } => Some((*thread, location.clone())),
            _ => None,
        }) {
            break loc;
        }
    };
    println!("✓ Breakpoint FIRED: thread=0x{:x} method_id=0x{:x} index={}", fired.0, fired.1.method_id, fired.1.index);
    assert_eq!(fired.1.method_id, m.method_id, "fired in the wrong method");

    println!("\n🎉 DEFERRED BREAKPOINT WORKS END TO END");
    conn.resume_all().await?;
    Ok(())
}
