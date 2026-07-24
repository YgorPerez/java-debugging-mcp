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
// Test static-field reads: classes_by_signature / all_classes / get_reference_values.
// Usage: cargo run --release --example test_static_field -- [port] [Simple.field ...]
// Defaults to port 8787 and reading ConfigDefaultUtils.{dsUrlMotor,dsInfra}.

use jdwp_client::types::ValueData;
use jdwp_client::JdwpConnection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut argv = std::env::args().skip(1);
    let port: u16 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(8787);

    println!("Connecting to localhost:{port}...");
    let mut conn = JdwpConnection::connect("localhost", port).await?;
    println!("✓ Connected\n");

    // 1) Fully-qualified lookup via classes_by_signature.
    let fqn = "Lbr/com/infotravel/util/ConfigDefaultUtils;";
    let by_sig = conn.classes_by_signature(fqn).await?;
    println!("classes_by_signature({fqn}) -> {} match(es)", by_sig.len());

    // 2) Simple-name lookup via all_classes (the new primitive backing bare-name resolution).
    let all = conn.all_classes().await?;
    println!("all_classes() -> {} loaded types", all.len());
    let simple = all
        .iter()
        .find(|c| c.ref_type_tag == 1 && c.signature.ends_with("/ConfigDefaultUtils;"))
        .cloned();
    let type_id = match (by_sig.first(), &simple) {
        (Some(c), _) => c.type_id,
        (None, Some(c)) => {
            println!("(FQN missed; resolved by simple name: {})", c.signature);
            c.type_id
        }
        (None, None) => {
            println!("❌ ConfigDefaultUtils not loaded yet — warm it up first");
            return Ok(());
        }
    };
    println!("type_id = 0x{type_id:x}\n");

    // 3) Read the requested static fields via get_reference_values.
    let fields: Vec<String> = {
        let rest: Vec<String> = argv.collect();
        if rest.is_empty() {
            vec!["dsUrlMotor".into(), "dsInfra".into(), "dsUrlIntegra".into()]
        } else {
            // Accept either "field" or "Class.field"; keep only the field name here.
            rest.iter().map(|s| s.rsplit('.').next().unwrap().to_string()).collect()
        }
    };

    let all_fields = conn.get_fields(type_id).await?;
    for fname in &fields {
        let f = all_fields.iter().find(|f| &f.name == fname && (f.mod_bits & 0x0008) != 0);
        let Some(f) = f else {
            println!("{fname:>14} = <no such static field>");
            continue;
        };
        let vals = conn.get_reference_values(type_id, vec![f.field_id]).await?;
        let rendered = match vals.into_iter().next().map(|v| v.data) {
            Some(ValueData::Object(0)) => "null".to_string(),
            Some(ValueData::Object(id)) => {
                conn.get_string_value(id).await.map(|s| format!("\"{s}\"")).unwrap_or_else(|_| format!("<obj 0x{id:x}>"))
            }
            Some(other) => format!("{other:?}"),
            None => "<no value>".to_string(),
        };
        println!("{fname:>14} = {rendered}");
    }

    Ok(())
}
