// Shared harness for the MCP-level integration tests.
//
// These tests drive the REAL `jdwp-mcp` binary over JSON-RPC on stdio against a real JVM, so they
// cover the server's handler glue — expression resolution, the event pump, deferred-breakpoint
// arming, session bookkeeping — which unit tests can't reach. Each test owns its own probe JVM and
// its own server process, so they can run concurrently.
//
// A JDK is required to compile and run the probes. There is no system JDK on every box (and CI may
// have none at all), so `Jdk::find` returns `None` and each test SKIPS rather than fails. Run them
// with:
//
//     scripts/integration-test.sh          # or: cargo test --test mcp_integration -- --ignored
//
// They are `#[ignore]`d because they spawn JVMs and take seconds, not milliseconds.

#![allow(dead_code)] // each test file uses a subset of this harness

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long to wait for a JVM event to show up before calling it a failure. Generous: the probes
/// loop on a ~150ms sleep, but a cold JVM plus class loading can take a second or two.
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(25);

/// Locations of the `java` / `javac` a test should use.
pub struct Jdk {
    pub java: PathBuf,
    pub javac: PathBuf,
}

impl Jdk {
    /// Find a JDK, or `None` if this machine has none (a JRE alone is not enough — the probes must
    /// be compiled). Checks `JAVA_HOME`, then `PATH`, then the `JetBrains` runtime that ships with a
    /// snap-installed `IntelliJ`, which is the only JDK on some dev boxes here.
    pub fn find() -> Option<Self> {
        if let Some(home) = std::env::var_os("JAVA_HOME") {
            let base = PathBuf::from(home).join("bin");
            let jdk = Self { java: base.join("java"), javac: base.join("javac") };
            if jdk.is_usable() {
                return Some(jdk);
            }
        }
        let on_path = Self { java: PathBuf::from("java"), javac: PathBuf::from("javac") };
        if Command::new(&on_path.javac).arg("-version").output().is_ok_and(|o| o.status.success()) {
            return Some(on_path);
        }
        // Newest snap revision first, so a stale one doesn't win.
        let mut candidates: Vec<PathBuf> = glob_snap_jbr();
        candidates.sort();
        candidates.reverse();
        candidates.into_iter().find_map(|bin| {
            let jdk = Self { java: bin.join("java"), javac: bin.join("javac") };
            jdk.is_usable().then_some(jdk)
        })
    }

    fn is_usable(&self) -> bool {
        self.java.exists() && self.javac.exists()
    }

    /// Compile `<repo>/examples/probes/<name>.java` into a fresh directory with `-g`.
    ///
    /// `-g` is not optional: without the local-variable table the JVM reports no locals, and every
    /// expression test that reads one silently has nothing to read.
    pub fn compile_probe(&self, name: &str, out_dir: &Path) -> Result<(), String> {
        let src = probe_source_path(name);
        let out = Command::new(&self.javac)
            .arg("-g")
            .arg("-d")
            .arg(out_dir)
            .arg(&src)
            .output()
            .map_err(|e| format!("failed to run javac: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("javac {} failed: {}", src.display(), String::from_utf8_lossy(&out.stderr)))
        }
    }
}

fn glob_snap_jbr() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/snap/intellij-idea-ultimate") else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join("jbr/bin"))
        .filter(|p| p.is_dir())
        .collect()
}

/// Absolute path to a checked-in probe's source.
pub fn probe_source_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/probes").join(format!("{name}.java"))
}

/// Read a probe's source. Tests use this to locate breakpoint lines by their `// BP<n>` markers
/// instead of hardcoding numbers, so editing the Java can't silently point a test at the wrong
/// statement.
pub fn probe_source(name: &str) -> String {
    let p = probe_source_path(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// 1-indexed line of `marker` in a probe's source.
pub fn probe_line(source: &str, marker: &str) -> i32 {
    source
        .lines()
        .position(|l| l.contains(marker))
        .map_or_else(
            || panic!("no `{marker}` marker in probe source"),
            |i| i32::try_from(i).unwrap_or(0) + 1,
        )
}

/// Ask the OS for a free TCP port by binding to port 0 and immediately releasing it.
///
/// Inherently racy — another process could take the port before the JVM binds it. Nothing portable
/// does better, since the JVM must open the port itself, and each test picking a fresh port keeps
/// concurrent tests from colliding, which is the failure that actually happened in practice.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map_or(0, |a| a.port())
}

/// A probe JVM running under JDWP, with its stdout captured.
///
/// Capturing stdout is what lets a test observe the *debuggee's own* behaviour — e.g. that the
/// caller of a `force_return`ed method really received the forced value — rather than only what the
/// debugger reports back about itself.
pub struct Probe {
    child: Child,
    stdin: Option<ChildStdin>,
    pub port: u16,
    lines: Arc<Mutex<Vec<String>>>,
    new_line: Receiver<()>,
    _dir: tempfile::TempDir,
}

impl Probe {
    /// Compile and launch `examples/probes/<name>.java`, waiting until it is accepting JDWP.
    pub fn launch(jdk: &Jdk, name: &str) -> Result<Self, String> {
        let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        jdk.compile_probe(name, dir.path())?;

        let port = free_port();
        // suspend=n so the probe runs immediately; the test attaches while it loops.
        let agent = format!("-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=127.0.0.1:{port}");
        let mut child = Command::new(&jdk.java)
            .arg(agent)
            .args(["-cp", "."])
            .arg(name)
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to launch probe {name}: {e}"))?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("probe has no stdout")?;
        let stderr = child.stderr.take().ok_or("probe has no stderr")?;
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (tx, new_line) = channel();

        // Reader threads are required, not a convenience: a full pipe blocks the JVM, which looks
        // exactly like a debugger deadlock. stderr is captured into the SAME buffer as stdout so an
        // uncaught Java exception shows up in the assertion message — discarding it once turned a
        // one-line `NoSuchMethodException` into a silent timeout with no clue what went wrong.
        pump(stdout, Arc::clone(&lines), tx.clone());
        pump(stderr, Arc::clone(&lines), tx);

        let probe = Self { child, stdin, port, lines, new_line, _dir: dir };
        probe.wait_until_listening()?;
        Ok(probe)
    }

    /// Block until the JDWP port accepts a connection, so `debug.attach` can't lose the race with a
    /// still-starting JVM.
    fn wait_until_listening(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                // dt_socket with server=y accepts ONE connection then stops listening, so that probe
                // connection is the one the server will use — hand the port over immediately.
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!("probe never listened on port {}", self.port))
    }

    /// Send a line to the probe's stdin (probes that wait for a cue to do something).
    pub fn send_line(&mut self, line: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("probe stdin already closed")?;
        writeln!(stdin, "{line}").map_err(|e| format!("probe stdin write: {e}"))?;
        stdin.flush().map_err(|e| format!("probe stdin flush: {e}"))
    }

    /// Every stdout line the probe has printed so far.
    pub fn output(&self) -> Vec<String> {
        self.lines.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Wait for a stdout line matching `pred`, returning it. `None` on timeout.
    pub fn wait_for_line(&self, timeout: Duration, mut pred: impl FnMut(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(hit) = self.output().into_iter().find(|l| pred(l)) {
                return Some(hit);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            // Woken per line, so this doesn't poll on a fixed tick.
            if self.new_line.recv_timeout(left.min(Duration::from_millis(250))).is_err()
                && Instant::now() >= deadline
            {
                return None;
            }
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Drain one of the probe's output streams into `sink`, notifying `tx` per line.
fn pump<R: std::io::Read + Send + 'static>(
    stream: R,
    sink: Arc<Mutex<Vec<String>>>,
    tx: std::sync::mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Ok(mut v) = sink.lock() {
                v.push(line);
            }
            if tx.send(()).is_err() {
                break; // the Probe was dropped
            }
        }
    });
}

/// A live `jdwp-mcp` child process spoken to over stdio JSON-RPC.
pub struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    /// Spawn the server binary Cargo just built for this test run (so it can never be a stale
    /// binary, which is the trap the example-based harnesses had) and complete `initialize`.
    pub fn start() -> Result<Self, String> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_jdwp-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to start jdwp-mcp: {e}"))?;
        let stdin = child.stdin.take().ok_or("server has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("server has no stdout")?);
        let mut server = Self { child, stdin, stdout, next_id: 1 };
        server.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "integration-test", "version": "0"}
            }),
        )?;
        Ok(server)
    }

    /// Send one request and return the response with the matching id, skipping anything else.
    //
    // `json!` interpolates its values through a reference, so clippy sees `params` as never consumed.
    // Taking it owned is still the right API: every caller builds a fresh `json!(...)` inline and has
    // no use for it afterwards, and `&json!(...)` at ~50 call sites buys nothing.
    #[allow(clippy::needless_pass_by_value)]
    pub fn request(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").map_err(|e| format!("server stdin: {e}"))?;
        self.stdin.flush().map_err(|e| format!("server flush: {e}"))?;
        let mut line = String::new();
        loop {
            line.clear();
            match self.stdout.read_line(&mut line) {
                Ok(0) => return Err("server closed stdout".to_string()),
                Ok(_) => {}
                Err(e) => return Err(format!("server stdout: {e}")),
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
            if v.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
                return Ok(v);
            }
        }
    }

    /// Call a `debug.*` tool and return its text content. A tool-level error comes back as text too
    /// (that is how MCP reports them), which is what lets tests assert on error messages.
    #[allow(clippy::needless_pass_by_value)] // see `request` above
    pub fn call(&mut self, tool: &str, args: serde_json::Value) -> String {
        match self.request("tools/call", serde_json::json!({"name": tool, "arguments": args})) {
            Ok(resp) => {
                if let Some(err) = resp.get("error") {
                    return format!("<rpc error> {err}");
                }
                resp["result"]["content"][0]["text"].as_str().unwrap_or("<no text>").to_string()
            }
            Err(e) => format!("<transport error> {e}"),
        }
    }

    /// Attach to a probe, panicking with the server's own message if it fails.
    pub fn attach(&mut self, port: u16) -> String {
        let out = self.call("debug.attach", serde_json::json!({"host": "127.0.0.1", "port": port}));
        assert!(out.contains("Connected"), "attach to port {port} failed: {out}");
        out
    }

    pub fn evaluate(&mut self, expr: &str) -> String {
        self.call("debug.evaluate", serde_json::json!({"expression": expr}))
    }

    pub fn last_event(&mut self) -> String {
        self.call("debug.get_last_event", serde_json::json!({}))
    }

    /// Clear every stop point and resume all threads.
    pub fn panic_reset(&mut self) -> String {
        self.call("debug.panic", serde_json::json!({}))
    }

    /// Poll `debug.get_last_event` until it reports something containing `needle`.
    ///
    /// `get_last_event` keeps returning the previous hit until a new one lands, so `needle` must be
    /// something the *expected* event has and no earlier one did — a distinct line number, or an
    /// event type not seen yet in this test.
    pub fn wait_for_event(&mut self, needle: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            let ev = self.last_event();
            if ev.contains(needle) {
                return Some(ev);
            }
        }
        None
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best-effort: unfreeze the debuggee before walking away, so a killed server can't leave a
        // suspended JVM behind.
        let _ = self.request("tools/call", serde_json::json!({"name": "debug.panic", "arguments": {}}));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Skip-with-a-reason guard. Returns the JDK, or `None` after printing why the test is skipped.
///
/// A missing JDK must not fail the suite — CI may have none — but a silent pass would hide that
/// nothing ran, so it says so on stdout (visible with `cargo test -- --nocapture`).
pub fn jdk_or_skip(test: &str) -> Option<Jdk> {
    let found = Jdk::find();
    if found.is_none() {
        println!("SKIP {test}: no JDK found (set JAVA_HOME or put javac on PATH)");
    }
    found
}

/// Assert `got` contains every string in `wants`, with a message naming the ones it doesn't.
pub fn assert_contains_all(label: &str, got: &str, wants: &[&str]) {
    let missing: Vec<&str> = wants.iter().copied().filter(|w| !got.contains(w)).collect();
    assert!(missing.is_empty(), "{label}: missing {missing:?}\n  got: {got}");
}
