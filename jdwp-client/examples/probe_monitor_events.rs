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
    clippy::panic,
    clippy::panic_in_result_fn
)]
// Empirical probe for DUMP-7 (#96): the four MONITOR_* events, and the one thing about them the JDWP
// spec states but this project has never measured — WHAT a `ClassOnly` (modKind 4) modifier actually
// tests on each kind. The spec says the monitor object's type for MONITOR_WAIT / MONITOR_WAITED and the
// location's type for the other two. If that is right, the same argument means two different things
// depending on the kind, and a tool description that called it "a filter on the lock's type" would be
// wrong for half the kinds.
//
// Also prints CapabilitiesNew bits 17 (canRequestMonitorEvents) and 18 (canGetMonitorFrameInfo), which
// the brief says to verify on JDK 11 rather than assume from the Temurin 17 vector in
// docs/heap-query-measurements.md.
//
// Drive it against examples/probes/MonitorProbe.java:
//   javac -g -d /tmp/mp examples/probes/MonitorProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8826 -cp /tmp/mp MonitorProbe
//   cargo run --release --example probe_monitor_events -- 8826

use jdwp_client::events::EventKind;
use jdwp_client::{EventFilters, JdwpConnection, MonitorKind, SuspendPolicy};
use std::time::{Duration, Instant};

/// How long to collect events for each arming under test.
const WINDOW: Duration = Duration::from_secs(3);

/// Count the events that arrive on `request` over [`WINDOW`], reporting them by lock class name.
async fn collect(conn: &mut JdwpConnection, label: &str) {
    // Drain whatever the previous arming left queued. Without this a window that should read 0 reads 1,
    // because the event loop buffers and a request cleared mid-stream leaves its last hits behind — which
    // is exactly the difference between "this filter matches nothing" and "this filter mostly works".
    while conn.try_recv_event().await.is_some() {}
    let deadline = Instant::now() + WINDOW;
    let mut n = 0usize;
    let mut by_lock: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    while Instant::now() < deadline {
        let Some(set) = conn.try_recv_event().await else {
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        };
        for e in &set.events {
            let monitor = match &e.details {
                EventKind::MonitorContendedEnter { monitor }
                | EventKind::MonitorContendedEntered { monitor }
                | EventKind::MonitorWait { monitor, .. }
                | EventKind::MonitorWaited { monitor, .. } => monitor,
                other => {
                    println!("   (non-monitor event: {other:?})");
                    continue;
                }
            };
            n += 1;
            let name = match conn.get_object_reference_type(monitor.monitor).await {
                Ok(t) => conn.get_signature(t).await.unwrap_or_else(|_| "<unreadable>".to_string()),
                Err(_) => "<unreadable>".to_string(),
            };
            *by_lock.entry(name).or_default() += 1;
            if let EventKind::MonitorWait { timeout, .. } = &e.details {
                *by_lock.entry(format!("   timeout={timeout}")).or_default() += 1;
            }
            if let EventKind::MonitorWaited { timed_out, .. } = &e.details {
                *by_lock.entry(format!("   timed_out={timed_out}")).or_default() += 1;
            }
        }
    }
    println!("{label}: {n} event(s) in {WINDOW:?}");
    for (k, v) in by_lock {
        println!("     {k} × {v}");
    }
}

async fn class_id(conn: &mut JdwpConnection, dotted: &str) -> u64 {
    let sig = format!("L{};", dotted.replace('.', "/"));
    let classes = conn.classes_by_signature(&sig).await.expect("lookup");
    let id = classes.first().map_or(0, |c| c.type_id);
    println!("   {dotted} -> type id 0x{id:x} ({} copy/copies)", classes.len());
    id
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args().nth(1).unwrap_or_else(|| "8826".into()).parse()?;
    let mut conn = JdwpConnection::connect("localhost", port).await?;
    println!("== connected on port {port}");

    let v = conn.get_version().await?;
    println!("== {} (JDWP {}.{})", v.vm_version, v.jdwp_major, v.jdwp_minor);

    let caps = conn.capabilities_new().await?;
    println!(
        "== canRequestMonitorEvents (17) = {}, canGetMonitorFrameInfo (18) = {}",
        caps.can_request_monitor_events, caps.can_get_monitor_frame_info
    );
    println!("== canGetInstanceInfo (16) = {} (sanity: DISC-10 measured true)", caps.can_get_instance_info);

    // --- all four kinds, unfiltered ------------------------------------------------------------
    let mut armed = Vec::new();
    for kind in MonitorKind::ALL {
        let req = conn
            .set_monitor_request(kind, SuspendPolicy::None, None, EventFilters::default())
            .await
            .expect("arm");
        println!("== armed {} (kind {}) as request {req}", kind.label(), kind.event_kind());
        armed.push((kind, req));
    }
    collect(&mut conn, "all four, unfiltered").await;
    for (kind, req) in armed {
        conn.clear_monitor_request(req, kind).await?;
    }

    // --- the ClassOnly question ----------------------------------------------------------------
    // MonitorProbe is the class every one of these events has its LOCATION in; FastLock and TimeoutLock
    // are the types of the monitor OBJECTS. If ClassOnly tested the location for every kind, filtering on
    // FastLock would yield nothing; if it tested the monitor for every kind, filtering on MonitorProbe
    // would yield nothing. The spec predicts one of each.
    println!("\n== resolving the types");
    let probe_class = class_id(&mut conn, "MonitorProbe").await;
    let fast_lock = class_id(&mut conn, "MonitorProbe$FastLock").await;
    let timeout_lock = class_id(&mut conn, "MonitorProbe$TimeoutLock").await;

    for (kind, name, type_id) in [
        (MonitorKind::Blocked, "location class (MonitorProbe)", probe_class),
        (MonitorKind::Blocked, "monitor class (FastLock)", fast_lock),
        (MonitorKind::Wait, "location class (MonitorProbe)", probe_class),
        (MonitorKind::Wait, "monitor class (TimeoutLock)", timeout_lock),
    ] {
        match conn
            .set_monitor_request(kind, SuspendPolicy::None, Some(type_id), EventFilters::default())
            .await
        {
            Ok(req) => {
                collect(&mut conn, &format!("{} + ClassOnly {name}", kind.label())).await;
                conn.clear_monitor_request(req, kind).await?;
            }
            Err(e) => println!("{} + ClassOnly {name}: REFUSED — {e}", kind.label()),
        }
    }

    // --- InstanceOnly, which ADR-0027 says to measure rather than trust -------------------------
    // The modifier tests the frame's `this`, and every method in MonitorProbe is STATIC — so `this` is
    // null and a non-null instance can never legitimately match. The question is only whether HotSpot
    // REFUSES it or accepts it and ignores it (ADR-0027's "inert" case), which decides whether
    // `debug.set_monitor_stop` can pass `instance_id` through or has to refuse it up front.
    //
    // A REAL object id, read off the probe's own static field: passing a reference-type id here would
    // measure nothing, because the two share an id space and the JVM would not complain either way.
    let fields = conn.get_fields(probe_class).await?;
    let fast_field = fields.iter().find(|f| f.name == "FAST").expect("MonitorProbe.FAST");
    let fast_obj = match conn.get_reference_values(probe_class, vec![fast_field.field_id]).await?.first() {
        Some(v) => match v.data {
            jdwp_client::types::ValueData::Object(id) => id,
            ref other => panic!("MonitorProbe.FAST is not a reference: {other:?}"),
        },
        None => panic!("no value for MonitorProbe.FAST"),
    };
    println!("\n== MonitorProbe.FAST (the lock OBJECT) = 0x{fast_obj:x}");
    match conn
        .set_monitor_request(
            MonitorKind::Blocked,
            SuspendPolicy::None,
            None,
            EventFilters { count: None, thread: None, instance: Some(fast_obj) },
        )
        .await
    {
        Ok(req) => {
            println!("== InstanceOnly ACCEPTED on a monitor request (request {req}) — is it APPLIED?");
            println!(
                "   (every frame here is static, so `this` is null: any FastLock event at all means inert,"
            );
            println!("    and a SlowLock/TimeoutLock event means it did not even scope to the named object)");
            collect(&mut conn, "blocked + InstanceOnly on the FastLock object").await;
            conn.clear_monitor_request(req, MonitorKind::Blocked).await?;
        }
        Err(e) => println!("== InstanceOnly REFUSED on a monitor request: {e}"),
    }

    Ok(())
}
