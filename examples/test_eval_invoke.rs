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
// End-to-end test of EVAL-1 (static-method invocation) + EVAL-2 (object arguments), driven through
// the REAL `jdwp-mcp` server over JSON-RPC on stdio — so it covers the handler glue
// (resolve_expression / resolve_static_head / find_method_for_args), not just the wire primitives.
//
// Probe: examples/probes/EvalProbe.java. Build the server, then:
//   cargo build --release
//   cd examples/probes && javac -g EvalProbe.java
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:8793 -cp . EvalProbe &
//   cargo run --release --example test_eval_invoke -- 8793
// The only argument is the port. Breakpoint lines are read from the probe's `// BP<n>` markers, so
// editing the Java file doesn't desync this harness.
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
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("<no text>");
        Ok(text.to_string())
    }

    fn evaluate(&mut self, expr: &str) -> Result<String, Box<dyn std::error::Error>> {
        self.call("debug.evaluate", serde_json::json!({"expression": expr}))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Assert that evaluating `expr` renders to something containing `want`.
fn expect(server: &mut Server, expr: &str, want: &str, failures: &mut Vec<String>) {
    let got = match server.evaluate(expr) {
        Ok(g) => g,
        Err(e) => format!("<harness error> {e}"),
    };
    if got.contains(want) {
        println!("  ✓ {got}");
    } else {
        println!("  ✗ {expr}\n      want to contain: {want}\n      got:             {got}");
        failures.push(expr.to_string());
    }
}

/// Clear everything, arm a conditional breakpoint at `line`, and assert it fires. The condition is
/// expected to hold on every loop iteration, so a miss means the condition failed to evaluate.
fn expect_condition(
    server: &mut Server,
    line: i32,
    condition: &str,
    failures: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    server.call("debug.panic", serde_json::json!({}))?;
    server.call(
        "debug.set_breakpoint",
        serde_json::json!({"class_pattern": "EvalProbe", "line": line, "condition": condition}),
    )?;
    let want = format!("\"line\":{line}");
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if server.call("debug.get_last_event", serde_json::json!({}))?.contains(&want) {
            println!("  ✓ `{condition}` fired at line {line}");
            return Ok(());
        }
    }
    println!("  ✗ `{condition}` never fired");
    failures.push(format!("condition {condition}"));
    Ok(())
}

/// 1-indexed line of the probe source carrying `// BP<n>`. Reading the markers instead of hardcoding
/// numbers means editing the Java file can't silently point the breakpoints at the wrong statements.
fn probe_line(source: &str, marker: &str) -> Result<i32, Box<dyn std::error::Error>> {
    source
        .lines()
        .position(|l| l.contains(marker))
        .map(|i| i32::try_from(i).unwrap_or(0) + 1)
        .ok_or_else(|| format!("no `{marker}` marker in EvalProbe.java").into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8791);

    let probe_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/probes/EvalProbe.java");
    let source = std::fs::read_to_string(&probe_src)
        .map_err(|e| format!("cannot read {}: {e}", probe_src.display()))?;
    let line = probe_line(&source, "// BP1")?;
    let cond_line_ref = probe_line(&source, "// BP2")?;
    let cond_line_num = probe_line(&source, "// BP3")?;

    // The server binary sits next to this example's own directory: target/<profile>/jdwp-mcp.
    let exe = std::env::current_exe()?;
    let bin = exe
        .parent()
        .and_then(|p| p.parent())
        .ok_or("cannot locate target dir")?
        .join("jdwp-mcp");
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
    println!("{}", server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "EvalProbe", "line": line}))?);

    // Wait for the loop in main() to reach work() and suspend there.
    println!("Waiting for the breakpoint to hit...");
    let mut hit = String::new();
    for _ in 0..60 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let ev = server.call("debug.get_last_event", serde_json::json!({}))?;
        if ev.contains("EvalProbe") {
            hit = ev;
            break;
        }
    }
    if hit.is_empty() {
        let _ = server.call("debug.panic", serde_json::json!({}));
        return Err("breakpoint never fired — is the probe running on this port?".into());
    }
    println!("{hit}\n");

    let mut failures: Vec<String> = Vec::new();

    // ---- EVAL-1: static-method invocation on a class-prefixed head ----
    println!("EVAL-1 — static methods:");
    expect(&mut server, "EvalProbe.twice(21)", "(int) 42", &mut failures);
    expect(&mut server, "EvalProbe.sum(1, 2)", "(int) 3", &mut failures);
    expect(&mut server, "EvalProbe.greet(\"world\")", "hello world", &mut failures);
    // Static field reads must keep working alongside the new call path.
    expect(&mut server, "EvalProbe.infra", "PROD", &mut failures);
    // A static call is a normal head, so the result keeps chaining.
    expect(&mut server, "EvalProbe.infraName().length()", "(int) 4", &mut failures);
    expect(&mut server, "EvalProbe.holder.label()", "holder#3", &mut failures);
    expect(&mut server, "EvalProbe.noSuchMethod(1)", "has no static method", &mut failures);

    // ---- EVAL-2: object arguments ----
    println!("\nEVAL-2 — object arguments:");
    // A local passed by reference to an instance method.
    expect(&mut server, "a.matches(b)", "(boolean) true", &mut failures);
    // A local passed to a static method.
    expect(&mut server, "EvalProbe.describe(a)", "item:alpha/1", &mut failures);
    // Two object args at once.
    expect(&mut server, "EvalProbe.sameName(a, b)", "(boolean) true", &mut failures);
    // A static-field sub-expression as an argument.
    expect(&mut server, "a.plus(EvalProbe.base)", "(int) 8", &mut failures);
    // A nested method call as an argument.
    expect(&mut server, "EvalProbe.twice(a.plus(1))", "(int) 4", &mut failures);
    // A field-path sub-expression as an argument.
    expect(&mut server, "EvalProbe.describe(EvalProbe.holder)", "item:holder/3", &mut failures);
    // `this` isn't available (work() is static) — the error must say so, not silently pass null.
    expect(&mut server, "a.matches(nosuchlocal)", "argument 'nosuchlocal'", &mut failures);

    // ---- Overload resolution by runtime type (both params are tag 'L') ----
    println!("\nOverload resolution — object args:");
    // pick(String) / pick(Item) / pick(Object) all take one reference param, so the tag alone
    // ('L' for every one of them) can't choose — only the argument's runtime class can.
    expect(&mut server, "EvalProbe.pick(a)", "Item:alpha", &mut failures);
    expect(&mut server, "EvalProbe.pick(\"x\")", "String:x", &mut failures);
    // pick(int) exists but is an INSTANCE method, and the three static overloads all take a
    // reference. Handing the JVM an int for a reference parameter SIGSEGVs the debuggee, so this
    // must be refused outright rather than fall back to an arity match.
    expect(&mut server, "EvalProbe.pick(9)", "has no static method", &mut failures);

    // ---- Conditions whose right-hand side is an expression, not a literal ----
    // Same generalization as method arguments: the RHS of a breakpoint condition is resolved in the
    // hit frame and compared value-to-value. Both conditions below are true on every iteration, so
    // failing to fire means the RHS didn't resolve, not that the run was unlucky.
    println!("\nConditional breakpoints with an expression right-hand side:");
    // Each condition gets its own line, so its hit is distinguishable from every earlier one by
    // line number — `debug.get_last_event` keeps reporting the previous hit until a new one lands.
    // Reference RHS: two distinct String objects with equal contents compare equal.
    expect_condition(&mut server, cond_line_ref, "a.name == b.name", &mut failures)?;
    // Numeric RHS, via a static call taking a local: check == local * 2.
    expect_condition(&mut server, cond_line_num, "check == EvalProbe.twice(local)", &mut failures)?;

    println!("\nCleaning up...");
    println!("{}", server.call("debug.panic", serde_json::json!({}))?);
    println!("{}", server.call("debug.disconnect", serde_json::json!({}))?);

    if failures.is_empty() {
        println!("\n🎉 STATIC INVOCATION + OBJECT ARGUMENTS WORK");
        Ok(())
    } else {
        Err(format!("{} expression(s) failed: {:?}", failures.len(), failures).into())
    }
}
