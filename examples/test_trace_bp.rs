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
// End-to-end test of the TRACE-1 logpoint mechanism at the library level:
//   set_breakpoint_ex(SuspendPolicy::EventThread) -> on hit, snapshot the top frame's args ->
//   resume_thread(hit thread only) -> the probe's loop keeps advancing, yielding N snapshots and
//   never leaving anything frozen. (The mcp-server event pump wraps exactly this into
//   debug.set_breakpoint{trace:true} + debug.get_traces.)
// Usage: cargo run --release --example test_trace_bp -- [port] [Class] [method]
// Defaults: 8790 TraceProbe tick  (pair with the TraceProbe probe, compiled with -g).

use std::time::Duration;

use jdwp_client::events::EventKind;
use jdwp_client::stackframe::VariableSlot;
use jdwp_client::types::ValueData;
use jdwp_client::{JdwpConnection, SuspendPolicy};

async fn next_event(conn: &JdwpConnection, secs: u64) -> Option<jdwp_client::EventSet> {
    match tokio::time::timeout(Duration::from_secs(secs), conn.recv_event()).await {
        Ok(ev) => ev,
        Err(_) => None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut argv = std::env::args().skip(1);
    let port: u16 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(8790);
    let class = argv.next().unwrap_or_else(|| "TraceProbe".to_string());
    let method = argv.next().unwrap_or_else(|| "tick".to_string());
    const TARGET: usize = 5;

    println!("Connecting to localhost:{port}...");
    let mut conn = JdwpConnection::connect("localhost", port).await?;
    println!("✓ Connected");

    let cid = conn.classes_by_signature(&format!("L{class};")).await?
        .first().map(|c| c.type_id).ok_or_else(|| format!("{class} not loaded"))?;
    let methods = conn.get_methods(cid).await?;
    let m = methods.iter().find(|m| m.name == method).ok_or("method not found")?;
    let entry = conn.get_line_table(cid, m.method_id).await?
        .lines.into_iter().min_by_key(|e| e.line_code_index).ok_or("no line table")?;

    // The crux of a logpoint: EventThread suspend policy — only the hit thread pauses, briefly.
    let req = conn.set_breakpoint_ex(cid, m.method_id, entry.line_code_index, SuspendPolicy::EventThread, None, None).await?;
    println!("✓ Trace breakpoint set on {class}.{method} (EventThread policy, request id {req})");

    let mut seen_i: Vec<i32> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    for _ in 0..(TARGET * 4) {
        let Some(ev) = next_event(&conn, 15).await else { break; };
        let Some((thread, loc)) = ev.events.iter().find_map(|e| match &e.details {
            EventKind::Breakpoint { thread, location } => Some((*thread, location.clone())),
            _ => None,
        }) else {
            conn.resume_all().await?;
            continue;
        };

        // Snapshot the top frame's in-scope locals/args.
        let frames = conn.get_frames(thread, 0, 1).await?;
        let frame = frames.first().ok_or("no frame at breakpoint")?;
        let vars = conn.get_variable_table(loc.class_id, loc.method_id).await?;
        let ci = loc.index;
        let in_scope: Vec<_> = vars.iter()
            .filter(|v| ci >= v.code_index && ci < v.code_index + v.length as u64)
            .collect();
        assert!(!in_scope.is_empty(), "no locals in scope — was the probe compiled with -g?");
        let slots: Vec<VariableSlot> = in_scope.iter()
            .map(|v| VariableSlot { slot: v.slot as i32, sig_byte: v.signature.as_bytes()[0] })
            .collect();
        let vals = conn.get_frame_values(thread, frame.frame_id, slots).await?;

        let mut i_val = None;
        let mut label_val = None;
        for (v, val) in in_scope.iter().zip(vals.iter()) {
            match (v.name.as_str(), &val.data) {
                ("i", ValueData::Int(n)) => i_val = Some(*n),
                ("label", ValueData::Object(id)) if *id != 0 => label_val = conn.get_string_value(*id).await.ok(),
                _ => {}
            }
        }
        println!("📢 trace hit: i={i_val:?} label={label_val:?} thread=0x{thread:x}");
        if let Some(i) = i_val { seen_i.push(i); }
        if let Some(l) = label_val { labels.push(l); }

        // Resume ONLY the hit thread — the loop keeps running; nothing left frozen.
        conn.resume_thread(thread).await?;
        if seen_i.len() >= TARGET { break; }
    }

    conn.clear_breakpoint(req).await?;
    conn.resume_all().await?;

    assert!(seen_i.len() >= TARGET, "expected >= {TARGET} trace hits, got {}", seen_i.len());
    // Strictly increasing => the loop advanced between hits, i.e. the thread was never left frozen.
    assert!(seen_i.windows(2).all(|w| w[1] > w[0]), "loop did not advance (frozen?): {seen_i:?}");
    assert!(labels.iter().all(|l| l.starts_with("iter-")), "unexpected label capture: {labels:?}");

    println!("\n🎉 TRACE/LOGPOINT WORKS: {} snapshots, i={seen_i:?} (increasing → never frozen)", seen_i.len());
    Ok(())
}
