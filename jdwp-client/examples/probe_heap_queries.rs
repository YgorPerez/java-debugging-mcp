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
    clippy::panic_in_result_fn,
    clippy::manual_unwrap_or_default
)]
// Empirical probe for the JDWP heap-query family this client does not implement:
//   ReferenceType.Instances          (2, 16)
//   VirtualMachine.InstanceCounts    (1, 21)
//   ObjectReference.ReferringObjects (9, 10)
//   Method.IsObsolete                (6, 4)
//   EventRequest.Set modKind 6 (ClassExclude) and 11 (InstanceOnly)
// plus the FULL VirtualMachine.CapabilitiesNew boolean vector.
//
// Drive it against scratchpad/HeapProbe.java.
// Usage: cargo run --release --example probe_heap_queries -- <port>

use jdwp_client::commands::{command_sets, event_commands, method_commands, vm_commands};
use jdwp_client::protocol::CommandPacket;
use jdwp_client::types::ValueData;
use jdwp_client::JdwpConnection;

const REF_TYPE_INSTANCES: u8 = 16;
const VM_INSTANCE_COUNTS: u8 = 21;
const OBJ_REFERRING_OBJECTS: u8 = 10;

const EVENT_KIND_EXCEPTION: u8 = 4;
const SUSPEND_POLICY_NONE: u8 = 0;

// ---------------------------------------------------------------- byte cursor

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn left(&self) -> usize {
        self.b.len() - self.p
    }
    fn u8(&mut self) -> u8 {
        let v = self.b[self.p];
        self.p += 1;
        v
    }
    fn i32(&mut self) -> i32 {
        let v = i32::from_be_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        v
    }
    fn i64(&mut self) -> i64 {
        let v = i64::from_be_bytes(self.b[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        v
    }
    fn u64(&mut self) -> u64 {
        let v = u64::from_be_bytes(self.b[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        v
    }
}

fn tag_name(t: u8) -> &'static str {
    match t {
        76 => "'L' OBJECT",
        91 => "'[' ARRAY",
        115 => "'s' STRING",
        116 => "'t' THREAD",
        103 => "'g' THREAD_GROUP",
        108 => "'l' CLASS_LOADER",
        99 => "'c' CLASS_OBJECT",
        _ => "??? UNKNOWN",
    }
}

fn err_name(c: u16) -> &'static str {
    match c {
        0 => "NONE",
        20 => "INVALID_OBJECT",
        21 => "INVALID_CLASS",
        23 => "INVALID_METHODID",
        99 => "NOT_IMPLEMENTED",
        103 => "ILLEGAL_ARGUMENT",
        112 => "VM_DEAD",
        502 => "INVALID_COUNT",
        _ => "(see spec)",
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
}

/// Decode `int n` followed by n `tagged-objectID` (byte tag + 8-byte objectID).
fn tagged_list(data: &[u8]) -> (i32, Vec<(u8, u64)>) {
    let mut c = Cur::new(data);
    let n = c.i32();
    let mut out = Vec::new();
    for _ in 0..n {
        if c.left() < 9 {
            break;
        }
        let tag = c.u8();
        let id = c.u64();
        out.push((tag, id));
    }
    (n, out)
}

fn summarize(list: &[(u8, u64)]) -> String {
    let mut kinds: Vec<(u8, usize)> = Vec::new();
    for (t, _) in list {
        match kinds.iter_mut().find(|(k, _)| k == t) {
            Some((_, n)) => *n += 1,
            None => kinds.push((*t, 1)),
        }
    }
    kinds.iter().map(|(t, n)| format!("{}x{} ", n, tag_name(*t))).collect()
}

// ---------------------------------------------------------------- main

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8787);
    let mut conn = JdwpConnection::connect("localhost", port).await?;
    println!("== connected on port {port}");

    let v = conn.get_version().await?;
    println!("VERSION jdwp={}.{} vm={} name={}", v.jdwp_major, v.jdwp_minor, v.vm_version, v.vm_name);

    // ------------------------------------------------ 1. CapabilitiesNew, full vector
    let id = conn.next_id();
    let reply = conn
        .send_command(CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::CAPABILITIES_NEW))
        .await?;
    let caps = reply.data().to_vec();
    println!("\n== CapabilitiesNew: err={} len={} bytes", reply.error_code, caps.len());
    const CAP_NAMES: [&str; 21] = [
        "canWatchFieldModification",
        "canWatchFieldAccess",
        "canGetBytecodes",
        "canGetSyntheticAttribute",
        "canGetOwnedMonitorInfo",
        "canGetCurrentContendedMonitor",
        "canGetMonitorInfo",
        "canRedefineClasses",
        "canAddMethod",
        "canUnrestrictedlyRedefineClasses",
        "canPopFrames",
        "canUseInstanceFilters",
        "canGetSourceDebugExtension",
        "canRequestVMDeathEvent",
        "canSetDefaultStratum",
        "canGetInstanceInfo",
        "canRequestMonitorEvents",
        "canGetMonitorFrameInfo",
        "canUseSourceNameFilters",
        "canGetConstantPool",
        "canForceEarlyReturn",
    ];
    for (i, b) in caps.iter().enumerate() {
        let pos = i + 1;
        let name = CAP_NAMES.get(i).copied().unwrap_or("reserved");
        println!("CAP {pos:>2} {name:<34} = {}", *b != 0);
    }

    // ------------------------------------------------ 2. resolve the probe types
    async fn lookup(conn: &mut JdwpConnection, sig: &str) -> Option<u64> {
        match conn.classes_by_signature(sig).await {
            Ok(v) if !v.is_empty() => Some(v[0].type_id),
            _ => None,
        }
    }

    let probe = lookup(&mut conn, "LHeapProbe;").await.expect("HeapProbe not loaded");
    let widget = lookup(&mut conn, "LHeapProbe$Widget;").await.expect("Widget not loaded");
    let subwidget = lookup(&mut conn, "LHeapProbe$SubWidget;").await.expect("SubWidget not loaded");
    let target = lookup(&mut conn, "LHeapProbe$Target;").await.expect("Target not loaded");
    let ballast = lookup(&mut conn, "LHeapProbe$Ballast;").await.expect("Ballast not loaded");
    let widget_arr = lookup(&mut conn, "[LHeapProbe$Widget;").await;
    let jstring = lookup(&mut conn, "Ljava/lang/String;").await.expect("String not loaded");
    let jthread = lookup(&mut conn, "Ljava/lang/Thread;").await.expect("Thread not loaded");

    let all = conn.all_classes().await?;
    let app_loader = all.iter().find(|c| c.signature.contains("AppClassLoader")).map(|c| c.type_id);
    println!(
        "\n== type ids: probe=0x{probe:x} widget=0x{widget:x} sub=0x{subwidget:x} target=0x{target:x} \
         ballast=0x{ballast:x} widgetArr={widget_arr:?} appLoader={app_loader:?}"
    );

    // The single Target instance, read out of the static field.
    let fields = conn.get_fields(probe).await?;
    let tf = fields.iter().find(|f| f.name == "TARGET").expect("no TARGET field");
    let target_obj = match conn.get_reference_values(probe, vec![tf.field_id]).await?.remove(0).data {
        ValueData::Object(id) => id,
        other => panic!("TARGET is not an object: {other:?}"),
    };
    println!("TARGET objectID = 0x{target_obj:x}");

    // ------------------------------------------------ 3. ReferenceType.Instances

    async fn instances(
        conn: &mut JdwpConnection,
        ref_type: u64,
        max: i32,
        label: &str,
    ) -> (u16, i32, Vec<(u8, u64)>, u128) {
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::REFERENCE_TYPE, REF_TYPE_INSTANCES);
        p.data.extend_from_slice(&ref_type.to_be_bytes());
        p.data.extend_from_slice(&max.to_be_bytes());
        let t0 = now_ms();
        let reply = conn.send_command(p).await.expect("send failed");
        let t1 = now_ms();
        let ec = reply.error_code;
        if ec != 0 {
            println!(
                "INSTANCES {label:<28} max={max:<4} -> ERROR {ec} {} [{}ms] (wall {t0}..{t1})",
                err_name(ec),
                t1 - t0
            );
            return (ec, 0, Vec::new(), t1 - t0);
        }
        let (n, list) = tagged_list(reply.data());
        println!(
            "INSTANCES {label:<28} max={max:<4} -> n={n:<8} bytes={:<10} [{}ms] (wall {t0}..{t1}) tags: {}",
            reply.data().len(),
            t1 - t0,
            summarize(&list)
        );
        (ec, n, list, t1 - t0)
    }

    println!("\n== ReferenceType.Instances (2,16)");
    // Expect 7 if instances() is exact-type, 9 if it includes subtypes.
    instances(&mut conn, widget, 0, "HeapProbe$Widget").await;
    instances(&mut conn, subwidget, 0, "HeapProbe$SubWidget").await;
    instances(&mut conn, widget, 3, "HeapProbe$Widget (clamp)").await;
    instances(&mut conn, widget, 1, "HeapProbe$Widget (clamp)").await;
    instances(&mut conn, widget, 100, "HeapProbe$Widget (over)").await;
    instances(&mut conn, widget, -1, "HeapProbe$Widget (neg)").await;
    instances(&mut conn, widget, i32::MIN, "HeapProbe$Widget (i32::MIN)").await;
    instances(&mut conn, target, 0, "HeapProbe$Target").await;
    instances(&mut conn, jstring, 5, "java.lang.String").await;
    instances(&mut conn, jthread, 0, "java.lang.Thread").await;
    if let Some(a) = widget_arr {
        instances(&mut conn, a, 0, "Widget[] (array type)").await;
    }
    if let Some(l) = app_loader {
        instances(&mut conn, l, 0, "AppClassLoader").await;
    }
    // A java.lang.Class instance: Instances on java.lang.Class should be tag 'c'.
    if let Some(cls) = lookup(&mut conn, "Ljava/lang/Class;").await {
        instances(&mut conn, cls, 4, "java.lang.Class").await;
    }
    // Bogus reference type id.
    instances(&mut conn, 0xDEAD_BEEF_u64, 0, "bogus refType").await;
    instances(&mut conn, 0, 0, "refType 0 (null)").await;

    // ------------------------------------------------ 4. VirtualMachine.InstanceCounts

    async fn instance_counts(
        conn: &mut JdwpConnection,
        types: &[u64],
        declared: Option<i32>,
        label: &str,
    ) -> (u16, Vec<i64>, u128) {
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, VM_INSTANCE_COUNTS);
        let n = declared.unwrap_or(types.len() as i32);
        p.data.extend_from_slice(&n.to_be_bytes());
        for t in types {
            p.data.extend_from_slice(&t.to_be_bytes());
        }
        let t0 = now_ms();
        let reply = conn.send_command(p).await.expect("send failed");
        let t1 = now_ms();
        if reply.error_code != 0 {
            println!(
                "COUNTS    {label:<28} -> ERROR {} {} [{}ms] (wall {t0}..{t1})",
                reply.error_code,
                err_name(reply.error_code),
                t1 - t0
            );
            return (reply.error_code, Vec::new(), t1 - t0);
        }
        let mut c = Cur::new(reply.data());
        let cnt = c.i32();
        let mut v = Vec::new();
        for _ in 0..cnt {
            if c.left() < 8 {
                break;
            }
            v.push(c.i64());
        }
        println!("COUNTS    {label:<28} -> counts={cnt} {v:?} [{}ms] (wall {t0}..{t1})", t1 - t0);
        (0, v, t1 - t0)
    }

    println!("\n== VirtualMachine.InstanceCounts (1,21)");
    instance_counts(&mut conn, &[widget], None, "[Widget]").await;
    instance_counts(&mut conn, &[subwidget], None, "[SubWidget]").await;
    instance_counts(&mut conn, &[widget, subwidget, target], None, "[Widget,Sub,Target]").await;
    instance_counts(&mut conn, &[ballast], None, "[Ballast] (2M)").await;
    instance_counts(&mut conn, &[widget, ballast, target], None, "[W,Ballast,T] one walk?").await;
    instance_counts(&mut conn, &[], Some(0), "refTypesCount=0").await;
    instance_counts(&mut conn, &[], Some(-1), "refTypesCount=-1").await;
    instance_counts(&mut conn, &[0xDEAD_BEEF_u64], None, "[bogus]").await;
    instance_counts(&mut conn, &[0], None, "[0]").await;

    // ------------------------------------------------ 5. ObjectReference.ReferringObjects

    async fn referring(
        conn: &mut JdwpConnection,
        obj: u64,
        max: i32,
        label: &str,
    ) -> (u16, i32, Vec<(u8, u64)>, u128) {
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::OBJECT_REFERENCE, OBJ_REFERRING_OBJECTS);
        p.data.extend_from_slice(&obj.to_be_bytes());
        p.data.extend_from_slice(&max.to_be_bytes());
        let t0 = now_ms();
        let reply = conn.send_command(p).await.expect("send failed");
        let t1 = now_ms();
        if reply.error_code != 0 {
            println!(
                "REFERRERS {label:<28} max={max:<4} -> ERROR {} {} [{}ms]",
                reply.error_code,
                err_name(reply.error_code),
                t1 - t0
            );
            return (reply.error_code, 0, Vec::new(), t1 - t0);
        }
        let (n, list) = tagged_list(reply.data());
        println!(
            "REFERRERS {label:<28} max={max:<4} -> n={n} [{}ms] (wall {t0}..{t1}) tags: {}",
            t1 - t0,
            summarize(&list)
        );
        for (t, oid) in &list {
            let rt = conn.get_object_reference_type(*oid).await.ok();
            let sig = match rt {
                Some(rt) => conn.get_signature(rt).await.unwrap_or_else(|_| "?".into()),
                None => "?".into(),
            };
            println!("            referrer tag={} id=0x{oid:x} type={sig}", tag_name(*t));
        }
        (0, n, list, t1 - t0)
    }

    println!("\n== ObjectReference.ReferringObjects (9,10)");
    referring(&mut conn, target_obj, 0, "TARGET (expect 3)").await;
    referring(&mut conn, target_obj, 1, "TARGET (clamp 1)").await;
    referring(&mut conn, target_obj, 99, "TARGET (over)").await;
    referring(&mut conn, target_obj, -1, "TARGET (neg)").await;
    referring(&mut conn, 0, 0, "objectID 0").await;
    referring(&mut conn, 0xDEAD_BEEF_u64, 0, "bogus objectID").await;

    // ------------------------------------------------ 6. Method.IsObsolete
    println!("\n== Method.IsObsolete (6,4)");
    let methods = conn.get_methods(probe).await?;
    for want in ["main", "tick"] {
        if let Some(m) = methods.iter().find(|m| m.name == want) {
            let id = conn.next_id();
            let mut p = CommandPacket::new(id, command_sets::METHOD, method_commands::IS_OBSOLETE);
            p.data.extend_from_slice(&probe.to_be_bytes());
            p.data.extend_from_slice(&m.method_id.to_be_bytes());
            let reply = conn.send_command(p).await?;
            println!(
                "ISOBSOLETE {want:<10} err={} {} replyLen={} value={:?}",
                reply.error_code,
                err_name(reply.error_code),
                reply.data().len(),
                reply.data().first().map(|b| *b != 0)
            );
        }
    }
    // Bogus method id, and a method id belonging to another class.
    {
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::METHOD, method_commands::IS_OBSOLETE);
        p.data.extend_from_slice(&probe.to_be_bytes());
        p.data.extend_from_slice(&0xDEAD_BEEF_u64.to_be_bytes());
        let reply = conn.send_command(p).await?;
        println!(
            "ISOBSOLETE bogus mid  err={} {} replyLen={}",
            reply.error_code,
            err_name(reply.error_code),
            reply.data().len()
        );
    }

    // ------------------------------------------------ 7. EventRequest.Set modifiers
    println!("\n== EventRequest.Set (15,1) modifiers");

    async fn clear(conn: &mut JdwpConnection, kind: u8, req: i32) {
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);
        p.data.push(kind);
        p.data.extend_from_slice(&req.to_be_bytes());
        let r = conn.send_command(p).await.expect("clear failed");
        println!("            cleared req={req} err={}", r.error_code);
    }

    // modKind 6, ClassExclude: byte modKind, string classPattern (int len + UTF-8, no NUL).
    {
        let pattern = "java.*";
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);
        p.data.push(EVENT_KIND_EXCEPTION);
        p.data.push(SUSPEND_POLICY_NONE);
        p.data.extend_from_slice(&1i32.to_be_bytes()); // modifiers
        p.data.push(6); // ClassExclude
        p.data.extend_from_slice(&(pattern.len() as i32).to_be_bytes());
        p.data.extend_from_slice(pattern.as_bytes());
        let reply = conn.send_command(p).await?;
        if reply.error_code == 0 {
            let req = Cur::new(reply.data()).i32();
            println!("MODKIND 6 ClassExclude \"{pattern}\" -> OK requestID={req}");
            clear(&mut conn, EVENT_KIND_EXCEPTION, req).await;
        } else {
            println!(
                "MODKIND 6 ClassExclude \"{pattern}\" -> ERROR {} {}",
                reply.error_code,
                err_name(reply.error_code)
            );
        }
    }

    // modKind 11, InstanceOnly: byte modKind, objectID instance.
    for (obj, label) in [(target_obj, "TARGET"), (0u64, "null(0)"), (0xDEAD_BEEF_u64, "bogus")] {
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);
        p.data.push(EVENT_KIND_EXCEPTION);
        p.data.push(SUSPEND_POLICY_NONE);
        p.data.extend_from_slice(&1i32.to_be_bytes());
        p.data.push(11); // InstanceOnly
        p.data.extend_from_slice(&obj.to_be_bytes());
        let reply = conn.send_command(p).await?;
        if reply.error_code == 0 {
            let req = Cur::new(reply.data()).i32();
            println!("MODKIND 11 InstanceOnly {label:<10} -> OK requestID={req}");
            clear(&mut conn, EVENT_KIND_EXCEPTION, req).await;
        } else {
            println!(
                "MODKIND 11 InstanceOnly {label:<10} -> ERROR {} {}",
                reply.error_code,
                err_name(reply.error_code)
            );
        }
    }

    // Both modifiers together, the realistic shape for a filtered trace.
    {
        let pattern = "java.*";
        let id = conn.next_id();
        let mut p = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);
        p.data.push(EVENT_KIND_EXCEPTION);
        p.data.push(SUSPEND_POLICY_NONE);
        p.data.extend_from_slice(&2i32.to_be_bytes());
        p.data.push(6);
        p.data.extend_from_slice(&(pattern.len() as i32).to_be_bytes());
        p.data.extend_from_slice(pattern.as_bytes());
        p.data.push(11);
        p.data.extend_from_slice(&target_obj.to_be_bytes());
        let reply = conn.send_command(p).await?;
        if reply.error_code == 0 {
            let req = Cur::new(reply.data()).i32();
            println!("MODKIND 6+11 combined -> OK requestID={req}");
            clear(&mut conn, EVENT_KIND_EXCEPTION, req).await;
        } else {
            println!("MODKIND 6+11 combined -> ERROR {} {}", reply.error_code, err_name(reply.error_code));
        }
    }

    // ------------------------------------------------ 8. cost on the inflated heap
    println!("\n== cost on the inflated heap (ballast is live)");
    for round in 0..3 {
        instance_counts(&mut conn, &[ballast], None, &format!("[Ballast] round {round}")).await;
    }
    // maxInstances=1 isolates heap-walk cost from reply size.
    instances(&mut conn, ballast, 1, "Ballast max=1 (walk only)").await;
    // maxInstances=0 on 2M objects: ~18MB reply.
    instances(&mut conn, ballast, 0, "Ballast max=0 (2M, big reply)").await;
    instances(&mut conn, widget, 0, "Widget after big walks").await;

    println!("\n== packets sent: {}", conn.packets_sent());
    println!("DONE {}", now_ms());
    Ok(())
}
