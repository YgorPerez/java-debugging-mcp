// Throwaway MCP integration harness (manual, ad-hoc) — not production code;
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
// End-to-end test of WATCH-1 field watchpoints, driven through the REAL `jdwp-mcp` server over
// JSON-RPC on stdio — so it covers the handler glue (handle_set_watchpoint, the FIELD_MODIFICATION /
// FIELD_ACCESS decode, describe_field_event's old→new read, and clear/panic awareness).
//
// Probe: examples/probes/WatchProbe.java.
//   cargo build --release
//   cd examples/probes && javac -g WatchProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8797 -cp . WatchProbe &
//   cargo run --release --example test_watchpoint -- 8797
//
// dt_socket server=y accepts ONE connection then stops listening, so use a fresh port per run.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// A live `jdwp-mcp` child process spoken to over stdio JSON-RPC.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    fn spawn(bin: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        Ok(Self { child, stdin, stdout, next_id: 1 })
    }

    /// Send one request and return the response with the matching id, skipping anything else.
    fn request(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}")?;
        self.stdin.flush()?;
        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line)? == 0 {
                return Err("server closed stdout".into());
            }
            let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
                return Ok(v);
            }
        }
    }

    /// Call a `debug.*` tool and return its text content (tool-level errors included as text).
    fn call(&mut self, tool: &str, args: serde_json::Value) -> Result<String, Box<dyn std::error::Error>> {
        let resp = self.request("tools/call", serde_json::json!({"name": tool, "arguments": args}))?;
        if let Some(err) = resp.get("error") {
            return Ok(format!("<rpc error> {err}"));
        }
        Ok(resp["result"]["content"][0]["text"].as_str().unwrap_or("<no text>").to_string())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn check(label: &str, got: &str, wants: &[&str], failures: &mut Vec<String>) {
    let missing: Vec<&str> = wants.iter().copied().filter(|w| !got.contains(w)).collect();
    if missing.is_empty() {
        println!("  ✓ {label}");
    } else {
        println!("  ✗ {label}\n      missing: {missing:?}\n      got:     {got}");
        failures.push(label.to_string());
    }
}

/// Clear everything, arm a watchpoint, and return the first hit `debug.get_last_event` reports.
/// Panics-then-rearms rather than reusing state, so each case starts from a known-quiet VM.
fn arm_and_wait(
    server: &mut Server,
    args: serde_json::Value,
    want_event: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    server.call("debug.panic", serde_json::json!({}))?;
    let set = server.call("debug.set_watchpoint", args)?;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let ev = server.call("debug.get_last_event", serde_json::json!({}))?;
        if ev.contains(want_event) {
            return Ok((set, ev));
        }
    }
    Ok((set, String::new()))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8797);

    let exe = std::env::current_exe()?;
    let bin = exe.parent().and_then(|p| p.parent()).ok_or("cannot locate target dir")?.join("jdwp-mcp");
    if !bin.exists() {
        return Err(format!("{} not found — run `cargo build --release` first", bin.display()).into());
    }

    println!("Starting {}...", bin.display());
    let mut server = Server::spawn(&bin)?;
    server.request(
        "initialize",
        serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    )?;
    println!("{}", server.call("debug.attach", serde_json::json!({"host": "localhost", "port": port}))?);

    let mut failures: Vec<String> = Vec::new();

    // ---- 1. Static field, modification ----
    // bumpCounter() does `counter = counter + 1`, so the hit must name that method and report an
    // old→new pair one apart. That the two differ at all proves the old value is read before the
    // pending store commits.
    println!("\nStatic field, modify watch (WatchProbe.counter):");
    let (set, ev) = arm_and_wait(
        &mut server,
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter"}),
        "field_modification",
    )?;
    check("set reports a watch_ id and the static kind", &set, &["watch_modify_", "static int"], &mut failures);
    check(
        "hit names the mutating method, the field, and static:true",
        &ev,
        &["\"event\":\"field_modification\"", "\"method\":\"bumpCounter\"", "\"field\":\"WatchProbe.counter\"", "\"static\":true"],
        &mut failures,
    );
    match parse_old_new(&ev) {
        Some((old, new)) if new == old + 1 => println!("  ✓ old → new is {old} → {new} (a single increment)"),
        Some((old, new)) => {
            println!("  ✗ old → new is {old} → {new}, expected new == old + 1");
            failures.push("counter old→new".to_string());
        }
        None => {
            println!("  ✗ could not parse old/new ints from: {ev}");
            failures.push("counter old→new".to_string());
        }
    }

    // ---- 2. Instance field, modification ----
    // relabel() alternates "even"/"odd" on holder.label, so old and new must be two different
    // strings and the event must carry the instance it happened on.
    println!("\nInstance field, modify watch (WatchProbe$Holder.label):");
    let (set, ev) = arm_and_wait(
        &mut server,
        serde_json::json!({"class_name": "WatchProbe$Holder", "field_name": "label"}),
        "field_modification",
    )?;
    check("set reports the instance kind", &set, &["watch_modify_", "instance java.lang.String"], &mut failures);
    check(
        "hit names relabel(), the field, static:false, and an instance id",
        &ev,
        &["\"method\":\"relabel\"", "\"field\":\"WatchProbe$Holder.label\"", "\"static\":false", "\"instance\":\"0x"],
        &mut failures,
    );
    let old_new_differ = ["even", "odd", "start"].iter().filter(|w| ev.contains(*w)).count() >= 2;
    if old_new_differ {
        println!("  ✓ old and new are two different label strings");
    } else {
        println!("  ✗ expected two distinct label strings in: {ev}");
        failures.push("label old→new".to_string());
    }

    // ---- 3. Access watch ----
    // readOnly is never written, so only an access watch can fire on it — and it reports a single
    // `value`, not an old→new pair.
    println!("\nAccess watch (WatchProbe.readOnly — read, never written):");
    let (set, ev) = arm_and_wait(
        &mut server,
        serde_json::json!({"class_name": "WatchProbe", "field_name": "readOnly", "modify": false, "access": true}),
        "field_access",
    )?;
    check("set reports an access watch id", &set, &["watch_access_"], &mut failures);
    check(
        "hit names readConfig(), the field, and a single value (no old/new)",
        &ev,
        &["\"event\":\"field_access\"", "\"method\":\"readConfig\"", "\"field\":\"WatchProbe.readOnly\"", "\"value\":\"(int) 41\""],
        &mut failures,
    );
    if ev.contains("\"old\"") || ev.contains("\"new\"") {
        println!("  ✗ an access hit must not report old/new: {ev}");
        failures.push("access reports old/new".to_string());
    }

    // ---- 4. list / clear / errors ----
    println!("\nBookkeeping:");
    server.call("debug.panic", serde_json::json!({}))?;
    let set = server.call(
        "debug.set_watchpoint",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "modify": true, "access": true}),
    )?;
    check("modify+access registers two ids", &set, &["watch_modify_", "watch_access_"], &mut failures);
    let listed = server.call("debug.list_breakpoints", serde_json::json!({}))?;
    check("list_breakpoints counts and shows them", &listed, &["2 watchpoint(s)", "👁", "WatchProbe.counter"], &mut failures);

    // Clear one by id; the other must survive.
    let one = set.lines().find_map(|l| l.split_whitespace().find(|w| w.starts_with("watch_modify_")))
        .map(|w| w.trim_end_matches(',').to_string())
        .ok_or("no watch_modify_ id in the set output")?;
    let cleared = server.call("debug.clear_breakpoint", serde_json::json!({"breakpoint_id": one}))?;
    check("clear_breakpoint removes a watchpoint by id", &cleared, &["Watchpoint cleared", "WatchProbe.counter"], &mut failures);
    let listed = server.call("debug.list_breakpoints", serde_json::json!({}))?;
    check("the other watchpoint survives", &listed, &["1 watchpoint(s)"], &mut failures);

    // panic must drop watchpoints too — ClearAllBreakpoints doesn't touch them.
    let panicked = server.call("debug.panic", serde_json::json!({}))?;
    check("panic reports and clears the rest", &panicked, &["watchpoint"], &mut failures);
    let listed = server.call("debug.list_breakpoints", serde_json::json!({}))?;
    check("nothing left after panic", &listed, &["No breakpoints set"], &mut failures);

    // Argument validation.
    let bad_field = server.call("debug.set_watchpoint", serde_json::json!({"class_name": "WatchProbe", "field_name": "nope"}))?;
    check("unknown field is a clear error", &bad_field, &["has no field 'nope'"], &mut failures);
    let bad_class = server.call("debug.set_watchpoint", serde_json::json!({"class_name": "NoSuchClass", "field_name": "x"}))?;
    check("unloaded class explains it can't be deferred", &bad_class, &["not loaded yet"], &mut failures);
    let neither = server.call(
        "debug.set_watchpoint",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "modify": false, "access": false}),
    )?;
    check("modify:false + access:false is rejected", &neither, &["at least one"], &mut failures);

    println!("\nCleaning up...");
    println!("{}", server.call("debug.panic", serde_json::json!({}))?);
    println!("{}", server.call("debug.disconnect", serde_json::json!({}))?);

    if failures.is_empty() {
        println!("\n🎉 FIELD WATCHPOINTS WORK (static + instance modify, access, list/clear/panic)");
        Ok(())
    } else {
        Err(format!("{} check(s) failed: {:?}", failures.len(), failures).into())
    }
}

/// Pull the ints out of `"old":"(int) 5"` / `"new":"(int) 6"` in an event line.
fn parse_old_new(ev: &str) -> Option<(i64, i64)> {
    let grab = |key: &str| -> Option<i64> {
        let at = ev.find(&format!("\"{key}\":\""))?;
        let rest = &ev[at..];
        let start = rest.find("(int) ")? + "(int) ".len();
        let end = rest[start..].find('"')? + start;
        rest[start..end].trim().parse().ok()
    };
    Some((grab("old")?, grab("new")?))
}
