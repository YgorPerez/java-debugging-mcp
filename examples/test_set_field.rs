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
// End-to-end test of SETF-1 live field writes:
//   ClassType.SetValues (statics) + ObjectReference.SetValues (instance), with untagged values.
// Writes a static String, a static int, then instance fields (int/String/boolean) on a static
// object, reading each back to confirm the new value landed. No suspended thread required — field
// writes work on a running VM.
// Usage: cargo run --release --example test_set_field -- [port]
// Defaults: 8789  (pair with the SetProbe probe).

use jdwp_client::extra::{value_bool, value_int};
use jdwp_client::types::ValueData;
use jdwp_client::JdwpConnection;

const ACC_STATIC: i32 = 0x0008;

async fn read_string_field_static(conn: &mut JdwpConnection, class_id: u64, field_id: u64) -> String {
    match conn.get_reference_values(class_id, vec![field_id]).await.ok().and_then(|v| v.into_iter().next()) {
        Some(v) => match v.data {
            ValueData::Object(0) => "null".to_string(),
            ValueData::Object(id) => {
                conn.get_string_value(id).await.unwrap_or_else(|_| format!("<obj 0x{id:x}>"))
            }
            other => format!("{other:?}"),
        },
        None => "<none>".to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut argv = std::env::args().skip(1);
    let port: u16 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(8789);

    println!("Connecting to localhost:{port}...");
    let mut conn = JdwpConnection::connect("localhost", port).await?;
    println!("✓ Connected");

    let class = conn
        .classes_by_signature("LSetProbe;")
        .await?
        .first()
        .map(|c| c.type_id)
        .ok_or("SetProbe not loaded")?;
    let fields = conn.get_fields(class).await?;
    let fid = |name: &str, want_static: bool| {
        fields
            .iter()
            .find(|f| f.name == name && ((f.mod_bits & ACC_STATIC) != 0) == want_static)
            .map(|f| f.field_id)
            .ok_or_else(|| format!("field {name} not found"))
    };

    // --- Static String: infra = "PROD" -> "DEVELOP" ---
    let infra_id = fid("infra", true)?;
    let before = read_string_field_static(&mut conn, class, infra_id).await;
    let dev = conn.create_string("DEVELOP").await?;
    conn.set_reference_values(class, vec![(infra_id, jdwp_client::extra::value_object(dev))]).await?;
    let after = read_string_field_static(&mut conn, class, infra_id).await;
    println!("static String infra: {before:?} -> {after:?}");
    assert_eq!(before, "PROD");
    assert_eq!(after, "DEVELOP");

    // --- Static int: counter = 0 -> 42 ---
    let counter_id = fid("counter", true)?;
    conn.set_reference_values(class, vec![(counter_id, value_int(42))]).await?;
    let counter_val =
        conn.get_reference_values(class, vec![counter_id]).await?.into_iter().next().map(|v| v.data);
    println!("static int counter -> {counter_val:?}");
    assert!(matches!(counter_val, Some(ValueData::Int(42))));

    // --- Instance fields on the static `holder` object ---
    let holder_id = fid("holder", true)?;
    let holder_obj =
        match conn.get_reference_values(class, vec![holder_id]).await?.into_iter().next().map(|v| v.data) {
            Some(ValueData::Object(id)) if id != 0 => id,
            other => return Err(format!("holder is not an object: {other:?}").into()),
        };
    let holder_type = conn.get_object_reference_type(holder_obj).await?;
    let hfields = conn.get_fields(holder_type).await?;
    let hfid = |name: &str| {
        hfields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.field_id)
            .ok_or_else(|| format!("holder field {name} not found"))
    };

    // int num = 10 -> 99
    let num_id = hfid("num")?;
    conn.set_object_values(holder_obj, vec![(num_id, value_int(99))]).await?;
    // String label = "orig" -> "changed"
    let label_id = hfid("label")?;
    let changed = conn.create_string("changed").await?;
    conn.set_object_values(holder_obj, vec![(label_id, jdwp_client::extra::value_object(changed))]).await?;
    // boolean flag = false -> true
    let flag_id = hfid("flag")?;
    conn.set_object_values(holder_obj, vec![(flag_id, value_bool(true))]).await?;

    // Read the instance fields back.
    let vals = conn.get_object_values(holder_obj, vec![num_id, label_id, flag_id]).await?;
    let num_after = vals.first().map(|v| v.data.clone());
    let flag_after = vals.get(2).map(|v| v.data.clone());
    let label_after = match vals.get(1).map(|v| v.data.clone()) {
        Some(ValueData::Object(id)) if id != 0 => conn.get_string_value(id).await.unwrap_or_default(),
        _ => String::new(),
    };
    println!("instance holder.num -> {num_after:?}");
    println!("instance holder.label -> {label_after:?}");
    println!("instance holder.flag -> {flag_after:?}");
    assert!(matches!(num_after, Some(ValueData::Int(99))));
    assert_eq!(label_after, "changed");
    assert!(matches!(flag_after, Some(ValueData::Boolean(true))));

    println!("\n🎉 LIVE FIELD WRITES WORK (static String/int + instance int/String/boolean)");
    Ok(())
}
