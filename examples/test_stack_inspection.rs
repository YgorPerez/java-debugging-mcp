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
// Test complete stack inspection flow

use jdwp_client::{JdwpConnection, SuspendPolicy};
use jdwp_client::stackframe::VariableSlot;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("jdwp_client=info")
        .init();

    println!("🔍 Stack Inspection Test\n");

    let mut conn = JdwpConnection::connect("localhost", 5005).await?;
    println!("✓ Connected to JVM\n");

    // Set up breakpoint on line 64 of HelloController.hello()
    let classes = conn.classes_by_signature("Lcom/example/probedemo/HelloController;").await?;
    let class = &classes[0];

    let methods = conn.get_methods(class.type_id).await?;
    let hello_method = methods.iter().find(|m| m.name == "hello").unwrap();

    let line_table = conn.get_line_table(class.type_id, hello_method.method_id).await?;
    let line_64 = line_table.lines.iter().find(|e| e.line_number == 64).unwrap();

    println!("🎯 Setting breakpoint at HelloController.java:64...");
    let request_id = conn.set_breakpoint(
        class.type_id,
        hello_method.method_id,
        line_64.line_code_index,
        SuspendPolicy::All,
    ).await?;
    println!("✓ Breakpoint set (request_id: {})\n", request_id);

    println!("💡 Now trigger the endpoint:");
    println!("   curl http://localhost:30080/\n");
    println!("⏳ Waiting for breakpoint to hit...");
    println!("   (Will check every 2 seconds for 30 seconds)\n");

    // Poll for suspended threads (simple approach - in real impl we'd listen for events)
    for _attempt in 1..=15 {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Get all threads
        let threads = conn.get_all_threads().await?;

        // Check if any thread is at our breakpoint by getting frames
        for thread_id in threads {
            match conn.get_frames(thread_id, 0, 1).await {
                Ok(frames) if !frames.is_empty() => {
                    let frame = &frames[0];

                    // Check if this is our breakpoint location
                    if frame.location.class_id == class.type_id
                        && frame.location.method_id == hello_method.method_id
                        && frame.location.index == line_64.line_code_index
                    {
                        println!("🎊 BREAKPOINT HIT!\n");
                        println!("📍 Location:");
                        println!("   Thread ID: {:x}", thread_id);
                        println!("   Frame ID: {:x}", frame.frame_id);
                        println!("   Class ID: {:x}", frame.location.class_id);
                        println!("   Method ID: {:x}", frame.location.method_id);
                        println!("   Bytecode index: {}\n", frame.location.index);

                        // Get variable table to know what variables exist
                        println!("🔬 Inspecting variables...");
                        let var_table = conn.get_variable_table(class.type_id, hello_method.method_id).await?;

                        // Find variables that are valid at this bytecode location
                        let current_index = frame.location.index;
                        let active_vars: Vec<_> = var_table
                            .iter()
                            .filter(|v| {
                                current_index >= v.code_index
                                    && current_index < v.code_index + v.length as u64
                            })
                            .collect();

                        println!("✓ Found {} active variables at this location:\n", active_vars.len());

                        // Build slots to request
                        let slots: Vec<VariableSlot> = active_vars
                            .iter()
                            .map(|v| VariableSlot {
                                slot: v.slot as i32,
                                sig_byte: v.signature.as_bytes()[0],
                            })
                            .collect();

                        // Get values!
                        let values = conn.get_frame_values(thread_id, frame.frame_id, slots).await?;

                        // Display variables
                        for (var, value) in active_vars.iter().zip(values.iter()) {
                            println!("   {} = {}", var.name, value.format());
                            println!("      signature: {}", var.signature);
                        }

                        println!("\n✅ Stack inspection complete!");
                        println!("\n🧹 Resuming execution...");
                        conn.resume_all().await?;
                        conn.clear_breakpoint(request_id).await?;
                        println!("✓ Cleaned up\n");

                        return Ok(());
                    }
                }
                _ => continue,
            }
        }

        print!(".");
        use std::io::Write;
        std::io::stdout().flush()?;
    }

    println!("\n\n⏱️  Timeout - breakpoint not hit");
    println!("   Make sure to: curl http://localhost:30080/");

    Ok(())
}
