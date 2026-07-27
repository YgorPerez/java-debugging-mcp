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
//
// Two exceptions, both deliberate, both following the same rule: a test that needs no JDK must not be
// hidden behind the flag that exists for tests that do (TEST-9, #25).
//
//  * `stdio_protocol.rs` uses the [`Server`] half alone to drive the process's JSON-RPC front door with
//    malformed input. No `Probe`, no JVM, no `#[ignore]`.
//  * The cassette tests in `mcp_integration.rs` drive the whole server against a **recorded** JDWP session
//    served out of a file — see [`cassette`] and ADR-0014. Same server, same handlers, same assertions as
//    the probe tests they were recorded from; no JVM anywhere.

#![allow(dead_code)] // each test file uses a subset of this harness

/// Recording a JDWP session to a file and serving it back with no JVM (TEST-12, #37). A child module rather
/// than more of this one: it is the only part of the harness a *reader* has to understand a file format for,
/// and it reaches into the proxy seam here (`Relay`, `wire_framed`, `read_frames`) rather than reimplementing
/// any of it — which was the whole condition #37 attached to building it.
pub mod cassette;

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
            let jdk = Self::in_bin(&PathBuf::from(home).join("bin"));
            if jdk.is_usable() {
                return Some(jdk);
            }
        }
        // No suffix here on purpose: this goes through `CreateProcessW`/`execvp` rather than an
        // existence check, and both resolve the platform's executable extension themselves.
        let on_path = Self { java: PathBuf::from("java"), javac: PathBuf::from("javac") };
        if Command::new(&on_path.javac).arg("-version").output().is_ok_and(|o| o.status.success()) {
            return Some(on_path);
        }
        // Newest snap revision first, so a stale one doesn't win.
        let mut candidates: Vec<PathBuf> = glob_snap_jbr();
        candidates.sort();
        candidates.reverse();
        candidates.into_iter().find_map(|bin| {
            let jdk = Self::in_bin(&bin);
            jdk.is_usable().then_some(jdk)
        })
    }

    /// The `java`/`javac` pair inside a JDK's `bin`, with the platform's executable suffix.
    ///
    /// The suffix is load-bearing rather than cosmetic: `is_usable` asks the filesystem, and on Windows
    /// the files are `java.exe` and `javac.exe`, so an unsuffixed path never exists. That made
    /// `Jdk::find` return `None` on a machine with a perfectly good JDK at `JAVA_HOME`, and because a
    /// missing JDK skips rather than fails, the entire `--ignored` suite reported `ok` in 0.00s while
    /// running nothing — the same shape as the SIGKILL coverage bug TEST-5 found.
    fn in_bin(bin: &std::path::Path) -> Self {
        const EXE: &str = if cfg!(windows) { ".exe" } else { "" };
        Self { java: bin.join(format!("java{EXE}")), javac: bin.join(format!("javac{EXE}")) }
    }

    fn is_usable(&self) -> bool {
        self.java.exists() && self.javac.exists()
    }

    /// Compile `<repo>/examples/probes/<name>.java` into a fresh directory with `-g`.
    ///
    /// `-g` is not optional: without the local-variable table the JVM reports no locals, and every
    /// expression test that reads one silently has nothing to read.
    ///
    /// `-encoding UTF-8` is not optional either, and the reason it looks optional is that JDK 18 hid it.
    /// Before JEP 400, `javac` reads source in the **platform** charset, which in a container with no
    /// locale set is US-ASCII — so every probe comment containing an em dash fails to compile with
    /// `unmappable character (0xE2)`. On JDK 21 the default is UTF-8 and the whole suite is green; on
    /// JDK 11 — which is what the shared 8180 runs — **50 of 53 tests failed to launch a probe** (TEST-11,
    /// #36). The sources are UTF-8, so saying so is correct on every JDK rather than a workaround for old
    /// ones.
    pub fn compile_probe(&self, name: &str, out_dir: &Path) -> Result<(), String> {
        self.compile_probe_with_debug_info("-g", name, out_dir)
    }

    /// Compile a probe with **`-g:none`** — no `SourceFile`, no line table, no local-variable table.
    ///
    /// The one deliberate exception to the paragraph above, and it does not weaken it: `-g` stays the
    /// default for every probe reached through [`compile_probe`](Self::compile_probe), and a caller has to
    /// name this one to get anything else. Exactly one probe does (`StrippedProbe`, TEST-14 #39), because
    /// until it existed `debug.source`'s `ABSENT_INFORMATION` branch was unreachable from this harness by
    /// construction — every probe carried the very attribute the branch is about the absence of.
    ///
    /// A probe compiled this way can be attached to and listed, and that is all: with no line-number table
    /// there is no line to hang a breakpoint on, and with no local-variable table there is nothing for an
    /// expression to read. That is not a limitation to work around — it is the condition being reproduced,
    /// and it is what a vendored jar on the shared 8180 actually looks like.
    pub fn compile_probe_stripped(&self, name: &str, out_dir: &Path) -> Result<(), String> {
        self.compile_probe_with_debug_info("-g:none", name, out_dir)
    }

    fn compile_probe_with_debug_info(
        &self,
        debug_info: &str,
        name: &str,
        out_dir: &Path,
    ) -> Result<(), String> {
        let src = probe_source_path(name);
        let out = Command::new(&self.javac)
            .arg(debug_info)
            .arg("-encoding")
            .arg("UTF-8")
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

/// Absolute path to a probe's checked-in JSR-45 SMAP — the fixture [`Probe::launch_with_smap`] installs,
/// found by the same name-is-the-convention rule as the probe's `.java`.
pub fn probe_smap_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/probes").join(format!("{name}.smap"))
}

/// Splice a JSR-45 `SourceDebugExtension` attribute carrying `smap` into an already-compiled class file.
///
/// **Why the harness has to do this itself.** Unlike `-g:none` there is no compiler flag to reach for:
/// `javac` cannot emit this attribute at all, so no Java that could be written into a probe produces one.
/// It is put there *after* compilation, by whatever generated the intermediate `.java` — which is not a
/// trick invented here but precisely what Jasper does to every JSP servlet it builds
/// (`SmapUtil$SDEInstaller`), and the reason `debug.source` asks for the attribute in the first place.
///
/// **What was weighed and dropped.** Running a real JSP or Kotlin compiler in the harness would produce a
/// genuine SMAP and drag in a toolchain larger than the thing under test. A checked-in `.class` fixture
/// avoids that at the price of an unreviewable binary blob, pinned to whichever JDK produced it. The
/// JDK's own class-file API (JEP 484, `java.lang.classfile`) is the tidy answer and is final only in 24+,
/// while this harness still has to build probes on the JDK 11 the shared 8180 runs (TEST-11, #36). What
/// is left is this: well-specified byte shuffling in the harness's own language, with the SMAP itself
/// checked in as readable text next to the probe rather than hidden inside a compiled artefact.
///
/// **Why a splice and not a rewrite.** A constant appended to the END of the pool leaves every index
/// already in the file pointing exactly where it did, and the class attribute table is the last thing in
/// a class file, so the attribute appends too. Three edits and nothing renumbers: bump
/// `constant_pool_count`, bump the class `attributes_count`, add the two new runs of bytes.
pub fn install_source_debug_extension(class_file: &Path, smap: &str) -> Result<(), String> {
    const ATTRIBUTE_NAME: &[u8] = b"SourceDebugExtension";

    // The attribute body is MODIFIED UTF-8, which is not Rust's UTF-8: NUL is two bytes and anything
    // outside the BMP is a surrogate pair. Every real SMAP is ASCII, where the two encodings agree, so
    // this refuses the case it would silently get wrong rather than emitting a class the JVM rejects.
    if !smap.is_ascii() {
        return Err(format!(
            "the SMAP for {} must be ASCII — the attribute body is MODIFIED UTF-8, which is not \
             Rust's, and this writes the bytes through unchanged",
            class_file.display()
        ));
    }

    let bytes = std::fs::read(class_file).map_err(|e| format!("cannot read {}: {e}", class_file.display()))?;
    if be_u32(&bytes, 0)? != 0xCAFE_BABE {
        return Err(format!("{} does not begin with 0xCAFEBABE", class_file.display()));
    }
    let pool_count = be_u16(&bytes, 8)?;
    let pool_end = constant_pool_end(&bytes)?;
    let attributes_at = class_attributes_count_offset(&bytes, pool_end)?;

    // The class attributes are the last thing in a class file, so a correct walk lands exactly on EOF.
    // Verified rather than assumed, because a walk that is off by anything does not fail here — it
    // splices into the middle of the method table, and the JVM then rejects the class with a message
    // about something else entirely, which is a bad afternoon to hand whoever adds the next probe.
    let walked = skip_attributes(&bytes, attributes_at)?;
    if walked != bytes.len() {
        return Err(format!(
            "walking {} landed on byte {walked} of {} — its layout is not what this splice assumes",
            class_file.display(),
            bytes.len()
        ));
    }

    // The new constant lands at the end of the pool, so it takes the index the OLD count named and
    // nothing already in the file has to move. Only the count itself can overflow, and only on a class
    // with 65534 constants already, which is a limit `javac` would have hit first.
    let new_pool_count = u16::try_from(pool_count + 1)
        .map_err(|_| "constant pool is full — no room for the attribute name".to_string())?;
    let name_index = new_pool_count - 1;
    let new_attribute_count = u16::try_from(be_u16(&bytes, attributes_at)? + 1)
        .map_err(|_| "class attribute table is full".to_string())?;
    let name_length = u16::try_from(ATTRIBUTE_NAME.len()).map_err(|_| "attribute name too long".to_string())?;
    let smap_length = u32::try_from(smap.len()).map_err(|_| "SMAP too long for a u4 length".to_string())?;

    let mut out = Vec::with_capacity(bytes.len() + smap.len() + ATTRIBUTE_NAME.len() + 16);
    out.extend_from_slice(&bytes[..8]);
    out.extend_from_slice(&new_pool_count.to_be_bytes());
    out.extend_from_slice(&bytes[10..pool_end]);
    out.push(CONSTANT_UTF8);
    out.extend_from_slice(&name_length.to_be_bytes());
    out.extend_from_slice(ATTRIBUTE_NAME);
    out.extend_from_slice(&bytes[pool_end..attributes_at]);
    out.extend_from_slice(&new_attribute_count.to_be_bytes());
    out.extend_from_slice(&bytes[attributes_at + 2..]);
    out.extend_from_slice(&name_index.to_be_bytes());
    out.extend_from_slice(&smap_length.to_be_bytes());
    out.extend_from_slice(smap.as_bytes());

    std::fs::write(class_file, &out).map_err(|e| format!("cannot write {}: {e}", class_file.display()))
}

/// `CONSTANT_Utf8_info`'s tag, the only one of the seventeen this needs to write.
const CONSTANT_UTF8: u8 = 1;

/// Offset of the class-level `attributes_count`, reached by walking everything in front of it.
fn class_attributes_count_offset(bytes: &[u8], pool_end: usize) -> Result<usize, String> {
    let mut at = pool_end;
    at += 6; // access_flags, this_class, super_class
    at += 2 + 2 * be_u16(bytes, at)?; // interfaces_count, then that many u2
    at = skip_members(bytes, at)?; // fields
    skip_members(bytes, at) // methods
}

/// Offset one past the last constant-pool entry.
fn constant_pool_end(bytes: &[u8]) -> Result<usize, String> {
    let count = be_u16(bytes, 8)?;
    let mut at = 10;
    let mut slot = 1;
    while slot < count {
        let tag = *bytes.get(at).ok_or_else(|| format!("class file ends at pool slot {slot}"))?;
        at += 1;
        at += match tag {
            CONSTANT_UTF8 => 2 + be_u16(bytes, at)?,  // a u2 length, then that many bytes
            7 | 8 | 16 | 19 | 20 => 2,                // Class, String, MethodType, Module, Package
            15 => 3,                                  // MethodHandle
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 4,  // Integer, Float, the refs, NameAndType, Dynamic
            5 | 6 => 8,                               // Long, Double
            other => return Err(format!("constant pool tag {other} at offset {at} is not one this knows")),
        };
        // Long and Double eat TWO slots each — a wart the JVMS itself calls "a poor choice" in a
        // footnote, and a walker that misses it drifts by one entry and then reads garbage as tags.
        slot += if matches!(tag, 5 | 6) { 2 } else { 1 };
    }
    Ok(at)
}

/// Skip a `field_info` or `method_info` table — identical shapes, so one walker does both.
fn skip_members(bytes: &[u8], mut at: usize) -> Result<usize, String> {
    let count = be_u16(bytes, at)?;
    at += 2;
    for _ in 0..count {
        at += 6; // access_flags, name_index, descriptor_index
        at = skip_attributes(bytes, at)?;
    }
    Ok(at)
}

/// Skip an `attributes` table, whichever of the four places it appears in.
fn skip_attributes(bytes: &[u8], mut at: usize) -> Result<usize, String> {
    let count = be_u16(bytes, at)?;
    at += 2;
    for _ in 0..count {
        at += 2; // attribute_name_index
        at += 4 + be_u32(bytes, at)?; // attribute_length, then that many bytes
    }
    Ok(at)
}

fn be_u16(bytes: &[u8], at: usize) -> Result<usize, String> {
    bytes
        .get(at..at + 2)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map(|b| usize::from(u16::from_be_bytes(b)))
        .ok_or_else(|| format!("class file ends inside the u2 at offset {at}"))
}

fn be_u32(bytes: &[u8], at: usize) -> Result<usize, String> {
    let raw = bytes
        .get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .ok_or_else(|| format!("class file ends inside the u4 at offset {at}"))?;
    usize::try_from(u32::from_be_bytes(raw))
        .map_err(|_| format!("the u4 at offset {at} does not fit an address on this platform"))
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

/// The half of a JDWP proxy that is the same whatever the proxy is *for*.
///
/// Bind a port the debugger attaches to instead of the debuggee's, accept its one connection, dial the
/// debuggee behind it, keep the live sockets so a blocked `read` can be woken, and take the whole thing
/// down on drop. Four modes now sit on this — a latency dial, a fault injector, a cassette recorder and a
/// cassette player — and until TEST-12 ([#37](https://github.com/YgorPerez/java-debugging-mcp/issues/37))
/// each new one arrived carrying its own copy of that paragraph. `7db6318` said so at the time and
/// deferred it on purpose: *a third user is the point to unify, not the second*. The recorder is the third.
///
/// **What the unification is careful NOT to swallow.** Only the socket lifecycle is shared. What each mode
/// does with the bytes is a closure, because the modes disagree about the one thing that matters:
/// [`LatencyRelay`] copies raw chunks and charges its delay per chunk, which is what ADR-0011's numbers were
/// measured through and what makes them a documented lower bound. Framing it — splitting a coalesced read
/// into packets and charging each one — would change the instrument, not just its implementation. So it
/// keeps [`pump_delayed`], and the three modes that must understand JDWP packets share [`wire_framed`]
/// instead. One seam, two pumps, and the reason for the second is written down rather than inherited.
///
/// `target_port` is `None` for a proxy with **nothing behind it** — the cassette player is a JDWP endpoint
/// rather than a middleman, and needs every line of this except the upstream connect.
struct Relay {
    /// The port a test attaches to instead of the probe's own.
    port: u16,
    /// Set on drop so the acceptor loop stops; the copy threads end on EOF.
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Live sockets, shut down on drop so a blocked `read` returns instead of leaking its thread.
    open: Arc<Mutex<Vec<std::net::TcpStream>>>,
}

impl Relay {
    /// Listen on a fresh port and hand each accepted connection — and the upstream one behind it, if
    /// there is an upstream — to `wire`, which is responsible for pumping the bytes.
    ///
    /// `label` only ever shows up in the two bind errors, and exists so a failure names the mode that
    /// failed rather than "relay" for all four.
    fn start(
        label: &'static str,
        target_port: Option<u16>,
        mut wire: impl FnMut(std::net::TcpStream, Option<std::net::TcpStream>) + Send + 'static,
    ) -> Result<Self, String> {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("{label} bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| format!("{label} addr: {e}"))?.port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let open: Arc<Mutex<Vec<std::net::TcpStream>>> = Arc::new(Mutex::new(Vec::new()));

        let (acc_stop, acc_open) = (Arc::clone(&stop), Arc::clone(&open));
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if acc_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let Ok(client) = incoming else { return };
                // dt_socket with server=y accepts one connection, so there is exactly one of these per
                // probe; connecting lazily (here, not at start) keeps the probe's single slot free until
                // the debugger actually attaches.
                let server = match target_port {
                    Some(p) => match std::net::TcpStream::connect(("127.0.0.1", p)) {
                        Ok(s) => Some(s),
                        Err(_) => return,
                    },
                    None => None,
                };
                // Nagle would add its own delay on top of the one being measured, which is the one thing
                // the latency mode must not do.
                let _ = client.set_nodelay(true);
                if let Some(s) = server.as_ref() {
                    let _ = s.set_nodelay(true);
                }
                if let Ok(mut v) = acc_open.lock() {
                    if let Ok(c) = client.try_clone() {
                        v.push(c);
                    }
                    if let Some(Ok(s)) = server.as_ref().map(std::net::TcpStream::try_clone) {
                        v.push(s);
                    }
                }
                wire(client, server);
            }
        });
        Ok(Self { port, stop, open })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Shutting the sockets down is what actually ends the copy threads: they are parked in a blocking
        // `read`, which a flag alone can never interrupt.
        if let Ok(v) = self.open.lock() {
            for s in v.iter() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
        // Unblock the acceptor's own `accept` by connecting to it once.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// A TCP relay in front of a probe's JDWP port that adds a round-trip delay to every forwarded chunk,
/// so a test can drive the debugger against a JVM that behaves like one **across a network hop**.
///
/// This exists because of what TEST-8 (#24) needed and could not get: every shared-instance default was
/// calibrated on loopback, and the reason given for not being able to re-calibrate was the real instance.
/// Two of the three variables that make the real thing different — thread count and stack depth — are
/// properties of the debuggee, so a probe can reproduce them exactly (see `PoolShapeProbe`). The third is
/// latency, and *that* is what this supplies. Kernel-level shaping (`tc qdisc … netem delay`) would be the
/// obvious tool and is unavailable in a container without `NET_ADMIN`; doing it in userspace also makes it
/// deterministic and portable, which a real network never is.
///
/// With it, "how does this behave on an instance 1ms away" stops being a question that needs the instance.
///
/// **What it models, and what it does not.** The delay is charged per forwarded *chunk*, not per JDWP
/// packet. Command/reply traffic is one packet per chunk — the client awaits each reply before sending the
/// next command — so for measuring a dump the two coincide. Traffic that arrives coalesced in one `read`
/// (a burst of events, or pipelined commands) shares a single delay and is therefore charged *less* than a
/// real network would charge it, so a measurement through this relay is a **lower bound** on the real
/// cost. It also models latency only: no jitter, no loss, no bandwidth limit.
pub struct LatencyRelay {
    /// The port a test should attach to instead of the probe's own.
    pub port: u16,
    /// The one-way delay in nanoseconds, read fresh by both copy threads before every write so
    /// [`set_rtt`](Self::set_rtt) can move the far end of the wire under a live connection.
    one_way_nanos: Arc<std::sync::atomic::AtomicU64>,
    /// Held only so dropping this drops the listener and the sockets with it.
    _relay: Relay,
}

impl LatencyRelay {
    /// Listen on a fresh port, forwarding to `target_port` with `rtt` added per round trip.
    ///
    /// `rtt` is the round trip, so each direction sleeps half of it — a caller thinking in terms of
    /// "an instance 2ms away" passes `Duration::from_millis(2)` and gets what they expect.
    pub fn start(target_port: u16, rtt: Duration) -> Result<Self, String> {
        let one_way = Arc::new(std::sync::atomic::AtomicU64::new(one_way_nanos(rtt)));
        let wire_delay = Arc::clone(&one_way);
        // The one mode that does NOT frame. See [`Relay`] for why that is deliberate rather than an
        // omission: this relay's numbers are in ADR-0011 and framing would change what it measures.
        let relay = Relay::start("relay", Some(target_port), move |client, server| {
            let Some(server) = server else { return };
            if let (Ok(c_read), Ok(s_read)) = (client.try_clone(), server.try_clone()) {
                pump_delayed(c_read, server, Arc::clone(&wire_delay));
                pump_delayed(s_read, client, Arc::clone(&wire_delay));
            }
        })?;
        Ok(Self { port: relay.port, one_way_nanos: one_way, _relay: relay })
    }

    /// Move the round trip on a **live** relay: the next chunk in either direction pays the new one.
    ///
    /// A dial rather than a constructor argument because of TEST-13 ([#38](
    /// https://github.com/YgorPerez/java-debugging-mcp/issues/38)). Measuring "0ms away" and "4ms away"
    /// used to mean two relays behind two attaches, so the two readings were separated by a JVM
    /// handshake and several seconds in which whatever else the box was doing could change its mind —
    /// and on a machine running the rest of this suite, it did. Turning the wire up and down under one
    /// connection puts both readings in the same few seconds of the same machine, which is the only
    /// thing that makes a difference between two wall clocks mean the wire.
    pub fn set_rtt(&self, rtt: Duration) {
        self.one_way_nanos.store(one_way_nanos(rtt), std::sync::atomic::Ordering::Relaxed);
    }
}

/// Half a round trip, in nanoseconds — what one direction of the relay sleeps.
fn one_way_nanos(rtt: Duration) -> u64 {
    u64::try_from((rtt / 2).as_nanos()).unwrap_or(u64::MAX)
}

/// Copy `from` → `to`, sleeping the relay's current one-way delay before each write. One thread per
/// direction.
///
/// The delay is loaded per chunk rather than captured once, because it is a dial the test can turn while
/// this thread is running — see [`LatencyRelay::set_rtt`].
fn pump_delayed(
    mut from: std::net::TcpStream,
    mut to: std::net::TcpStream,
    delay: Arc<std::sync::atomic::AtomicU64>,
) {
    std::thread::spawn(move || {
        // Big enough for a `Frames` or `Methods` reply on a deep stack, so a single logical reply is not
        // split into several chunks and charged the delay more than once.
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = match std::io::Read::read(&mut from, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let one_way = Duration::from_nanos(delay.load(std::sync::atomic::Ordering::Relaxed));
            if !one_way.is_zero() {
                std::thread::sleep(one_way);
            }
            if std::io::Write::write_all(&mut to, buf.get(..n).unwrap_or_default()).is_err() {
                break;
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Both);
    });
}

/// What a [`FaultRelay`] does to a JDWP reply on its way back to the debugger.
#[derive(Clone, Debug)]
pub enum Fault {
    /// Replace the reply with a JDWP error reply carrying this code and no payload — what a JVM sends
    /// when it cannot answer (`NOT_IMPLEMENTED` 99, `ABSENT_INFORMATION` 101, `INVALID_OBJECT` 20 …).
    Error(u16),
    /// Replace the reply's payload with these bytes, recomputing the length header. Used to make the JVM
    /// *lie* rather than fail: a `SuspendCount` that never reaches zero, a `Version` that claims JDWP 1.5,
    /// or a payload too short for what the reader expects.
    Payload(Vec<u8>),
}

/// A JDWP proxy that rewrites chosen replies, so the debugger can be driven through failures a healthy
/// `HotSpot` will not produce on demand.
///
/// Some of this codebase's most important branches are the ones that report bad news, and several were
/// unreachable from outside. TODO.md's coverage review names the worst case: `resume_all_fully`'s
/// "the VM is STILL suspended" tail, which needs a suspend depth above `MAX_RESUME_ATTEMPTS`, and
/// **cannot be built by any sequence of this tool's own calls** because `debug.pause` is idempotent
/// (ADR-0003). That branch is the entire point of ADR-0003 — a resume that verifies instead of assuming —
/// and it had never once executed. Rewriting `ThreadReference.SuspendCount` to a count that never falls
/// reaches it in one test.
///
/// Same seam as [`LatencyRelay`], used differently: sit in the middle of the JDWP stream and change what
/// arrives. Unlike that one this must **frame** the protocol, because a reply carries only the request id —
/// which command it answers is known only from the request that went the other way. That framing is now
/// [`wire_framed`], shared with the cassette recorder (ADR-0014); this type is the fault policy on top of it.
pub struct FaultRelay {
    /// The port a test attaches to instead of the probe's own.
    pub port: u16,
    /// Held only so dropping this drops the listener and the sockets with it.
    _relay: Relay,
}

/// The JDWP handshake both ends send before any packet framing begins.
const JDWP_HANDSHAKE: &[u8] = b"JDWP-Handshake";

/// Every JDWP packet starts with length(4) + id(4) + flags(1), then either error(2) or set+cmd(2).
const JDWP_HEADER: usize = 11;

/// Set in a packet's flags byte when it is a reply rather than a command.
const JDWP_REPLY_FLAG: u8 = 0x80;

impl FaultRelay {
    /// Listen on a fresh port, forwarding to `target_port` and applying `faults` — keyed by
    /// `(command set, command)` — to every reply answering a matching command.
    pub fn start(target_port: u16, faults: Vec<(u8, u8, Fault)>) -> Result<Self, String> {
        let relay = Relay::start("fault relay", Some(target_port), move |client, server| {
            let Some(server) = server else { return };
            let faults = faults.clone();
            wire_framed(client, server, move |seen| {
                // Composite events arrive on this side too and are *command* packets (set 64), not
                // replies; they answer nothing we asked and fall through untouched. Faulting them would
                // break the event pump rather than test it.
                let FromDebuggee::Reply { command, reply, .. } = seen else { return None };
                let id = packet_id(reply)?;
                let fault = faults.iter().find(|(s, c, _)| (*s, *c) == command).map(|(_, _, f)| f)?;
                Some(match fault {
                    Fault::Error(code) => reply_packet(id, *code, &[]),
                    Fault::Payload(p) => reply_packet(id, 0, p),
                })
            });
        })?;
        Ok(Self { port: relay.port, _relay: relay })
    }
}

/// Read whole JDWP packets out of `buf`, returning them and leaving any partial tail behind.
///
/// Framing is length-prefixed, so a chunk boundary can fall anywhere; a proxy that assumed one read is
/// one packet would corrupt the stream under load rather than fail visibly.
fn take_packets(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    // The length is copied out rather than borrowed, so the drain below is free to take the buffer.
    while let Some(head) = buf.get(..4).and_then(|h| <[u8; 4]>::try_from(h).ok()) {
        let len = u32::from_be_bytes(head) as usize;
        // A length below the header size would be a protocol violation; stop rather than loop forever.
        if len < JDWP_HEADER || buf.len() < len {
            break;
        }
        out.push(buf.drain(..len).collect());
    }
    out
}

/// One unit of the JDWP stream, as [`read_frames`] hands it over.
///
/// The handshake is called out rather than folded into the packet case because it is not a packet: it is
/// fourteen bare bytes in front of the framing, and every mode has to get past it before anything else it
/// does makes sense. A proxy that treated it as a packet would read its first four bytes (`JDWP`) as a
/// length of 1 246 906 704 and wait forever.
enum Frame<'a> {
    Handshake(&'a [u8]),
    Packet(&'a [u8]),
}

/// Read one end of a JDWP connection: the handshake, then whole packets, one call to `on_frame` each.
/// Returns when the peer hangs up, or as soon as `on_frame` answers `false`.
///
/// **The single framing implementation in this harness.** Fault injection, cassette recording and cassette
/// replay all come through here; TEST-12 (#37) called adding a third copy the thing not to do.
fn read_frames(mut from: std::net::TcpStream, mut on_frame: impl FnMut(Frame<'_>) -> bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    let mut shaken = false;
    loop {
        let n = match std::io::Read::read(&mut from, &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        if !shaken {
            if buf.len() < JDWP_HANDSHAKE.len() {
                continue;
            }
            let shake: Vec<u8> = buf.drain(..JDWP_HANDSHAKE.len()).collect();
            if !on_frame(Frame::Handshake(&shake)) {
                return;
            }
            shaken = true;
        }
        for pkt in take_packets(&mut buf) {
            if !on_frame(Frame::Packet(&pkt)) {
                return;
            }
        }
    }
}

/// Copy one framed direction, giving `transform` the chance to substitute each packet. `None` forwards the
/// original untouched, which is what every packet nobody is interested in gets.
///
/// The returned flag goes `true` when this direction has ended — the recorder needs to know the debugger
/// has hung up before it writes a cassette, and the alternative (guessing with a sleep) is how a recording
/// silently loses its last exchange.
fn pump_framed(
    from: std::net::TcpStream,
    mut to: std::net::TcpStream,
    mut transform: impl FnMut(&[u8]) -> Option<Vec<u8>> + Send + 'static,
) -> Arc<std::sync::atomic::AtomicBool> {
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    std::thread::spawn(move || {
        read_frames(from, |frame| match frame {
            // Requests are never modified: the debugger's own traffic is not what is under test.
            Frame::Handshake(b) => std::io::Write::write_all(&mut to, b).is_ok(),
            Frame::Packet(p) => {
                let out = transform(p);
                std::io::Write::write_all(&mut to, out.as_deref().unwrap_or(p)).is_ok()
            }
        });
        let _ = to.shutdown(std::net::Shutdown::Both);
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    finished
}

/// What a framing proxy sees coming back from the debuggee.
///
/// The distinction is the whole reason framing is needed. A reply names no command — only the id it
/// answers — so pairing it with the request that went the other way is the only way to know what it is a
/// reply *to*. An event names no id of ours at all, because nobody asked for it.
enum FromDebuggee<'a> {
    /// A reply, with the `(command set, command)` and request payload it answers.
    Reply { command: (u8, u8), request: &'a [u8], reply: &'a [u8] },
    /// The debuggee speaking unprompted — a composite event packet.
    Event(&'a [u8]),
}

/// Wire both directions of a **framing** proxy, calling `on_reply` for everything the debuggee sends back.
/// `on_reply` returns a replacement packet, or `None` to forward what arrived.
///
/// Returns the flag from [`pump_framed`] for the debuggee direction: `true` once the debuggee side has
/// closed, which is the last moment anything can still be recorded.
fn wire_framed(
    client: std::net::TcpStream,
    server: std::net::TcpStream,
    mut on_reply: impl FnMut(FromDebuggee<'_>) -> Option<Vec<u8>> + Send + 'static,
) -> Arc<std::sync::atomic::AtomicBool> {
    // Which command — and which request payload — each id belongs to, learned from the request direction
    // and read by the reply direction. A reply carries neither, so this map is the only way to key one.
    let pending: Arc<Mutex<std::collections::HashMap<u32, (u8, u8, Vec<u8>)>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (Ok(c_read), Ok(s_read)) = (client.try_clone(), server.try_clone()) else {
        return Arc::new(std::sync::atomic::AtomicBool::new(true));
    };

    let outbound = Arc::clone(&pending);
    pump_framed(c_read, server, move |pkt| {
        if let (Some(id), Some(flags), Some(set), Some(cmd)) =
            (packet_id(pkt), pkt.get(8).copied(), pkt.get(9).copied(), pkt.get(10).copied())
        {
            if flags & JDWP_REPLY_FLAG == 0 {
                if let Ok(mut m) = outbound.lock() {
                    m.insert(id, (set, cmd, pkt.get(JDWP_HEADER..).unwrap_or_default().to_vec()));
                }
            }
        }
        None
    });

    pump_framed(s_read, client, move |pkt| {
        if pkt.get(8).copied().is_some_and(|f| f & JDWP_REPLY_FLAG == 0) {
            return on_reply(FromDebuggee::Event(pkt));
        }
        let id = packet_id(pkt)?;
        // Taken, not read: an id is answered once, so leaving it would grow the map for the whole session.
        let (set, cmd, request) = pending.lock().ok()?.remove(&id)?;
        on_reply(FromDebuggee::Reply { command: (set, cmd), request: &request, reply: pkt })
    })
}

/// A JDWP reply packet: length, id, the reply flag, an error code, and a payload.
fn reply_packet(id: u32, error: u16, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(JDWP_HEADER + payload.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(JDWP_HEADER + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.push(JDWP_REPLY_FLAG);
    out.extend_from_slice(&error.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A packet's request id, from bytes 4..8.
fn packet_id(pkt: &[u8]) -> Option<u32> {
    let b = pkt.get(4..8)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// A JDWP string as it appears in a reply payload: a 4-byte length then UTF-8 bytes. For hand-building
/// the payloads a [`Fault::Payload`] substitutes.
pub fn jdwp_string(s: &str) -> Vec<u8> {
    let mut out = u32::try_from(s.len()).unwrap_or(0).to_be_bytes().to_vec();
    out.extend_from_slice(s.as_bytes());
    out
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
    /// How long to wait for a freshly launched probe to accept a JDWP connection.
    ///
    /// 30s was enough for every warm run measured (a probe binds in about a second) but not for a
    /// cold one: the first launch after a JDK install spent over 80s being virus-scanned on Windows
    /// and timed out here, which read as a broken probe rather than a slow disk. 90s costs nothing
    /// when things are working — the wait ends as soon as the port answers — and only lengthens the
    /// genuinely-broken case, which now at least explains itself.
    const PROBE_LISTEN_TIMEOUT: Duration = Duration::from_secs(90);

    /// Compile and launch `examples/probes/<name>.java`, waiting until it is accepting JDWP.
    pub fn launch(jdk: &Jdk, name: &str) -> Result<Self, String> {
        Self::launch_built_by(jdk, name, Jdk::compile_probe)
    }

    /// Like [`launch`](Self::launch) but compiled `-g:none` — see [`Jdk::compile_probe_stripped`] for
    /// why exactly one probe wants that, and why it does not weaken the default for the rest.
    pub fn launch_stripped(jdk: &Jdk, name: &str) -> Result<Self, String> {
        Self::launch_built_by(jdk, name, Jdk::compile_probe_stripped)
    }

    /// Like [`launch`](Self::launch), but with the probe's checked-in `<name>.smap` installed into
    /// `<name>.class` as a JSR-45 `SourceDebugExtension` before the JVM ever loads it — the state a
    /// JSP-derived servlet is in by the time it reaches the shared 8180 (TEST-15, #40).
    ///
    /// Only the probe's own class is patched. Any other class in the same file is left exactly as
    /// `javac` produced it, which is what gives a test a control compiled in the same breath.
    pub fn launch_with_smap(jdk: &Jdk, name: &str) -> Result<Self, String> {
        Self::launch_built_by(jdk, name, |jdk, name, dir| {
            jdk.compile_probe(name, dir)?;
            let fixture = probe_smap_path(name);
            let smap = std::fs::read_to_string(&fixture)
                .map_err(|e| format!("cannot read {}: {e}", fixture.display()))?;
            install_source_debug_extension(&dir.join(format!("{name}.class")), &smap)
        })
    }

    /// Launch a probe whose class files `build` is responsible for putting in the run directory.
    ///
    /// The seam exists because two of `debug.source`'s branches are about what the **class file** says
    /// rather than what the Java said, and no amount of different Java reaches them: one needs a different
    /// `javac` flag (TEST-14, #39), the other an attribute `javac` has no option to emit at all (TEST-15,
    /// #40). Everything downstream of the class files is identical — the port, the agent argument, the
    /// reader threads, the listen wait — so it stays in one place rather than being copied per variant.
    fn launch_built_by(
        jdk: &Jdk,
        name: &str,
        build: impl FnOnce(&Jdk, &str, &Path) -> Result<(), String>,
    ) -> Result<Self, String> {
        let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        build(jdk, name, dir.path())?;

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
        let started = Instant::now();
        let deadline = started + Self::PROBE_LISTEN_TIMEOUT;
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                // dt_socket with server=y accepts ONE connection then stops listening, so that probe
                // connection is the one the server will use — hand the port over immediately.
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Say what the probe said. The reader threads have been capturing stdout AND stderr this whole
        // time, and on the two failures that actually happen — the JVM refusing the agent argument, and
        // a Java exception before main gets going — the reason is sitting right there. Reporting only
        // "never listened" throws it away and leaves a timeout with nothing to go on, which is the same
        // mistake `pump` already documents for the stderr case.
        let captured = self.output();
        let tail: Vec<&String> = captured.iter().rev().take(10).rev().collect();
        let said = if tail.is_empty() {
            "it printed nothing at all".to_string()
        } else {
            format!("it printed:\n  {}", tail.iter()
                .map(|l| l.as_str()).collect::<Vec<_>>().join("\n  "))
        };
        Err(format!(
            "probe never listened on port {} within {:?} (waited {:?}) — {said}\n\
             If it printed nothing, the JVM is probably just slow to start rather than broken: on \
             Windows a first run after a JDK is installed or updated can spend longer than this being \
             scanned by Defender, and the same probe then launches in ~1s once warm.",
            self.port, Self::PROBE_LISTEN_TIMEOUT, started.elapsed(),
        ))
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
    /// `Option` so `Drop` can *close* it (by dropping it) to shut the server down cleanly, rather than
    /// reaching for `kill()`. See the `Drop` impl for why that matters.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    /// Spawn the server binary Cargo just built for this test run (so it can never be a stale
    /// binary, which is the trap the example-based harnesses had) and complete `initialize`.
    pub fn start() -> Result<Self, String> {
        Self::start_with_env(&[])
    }

    /// Like [`start`](Self::start), but with extra environment variables set on the server process —
    /// e.g. `JDWP_WATCHDOG_SECS=1` for the watchdog tests or `JDWP_READONLY=1` for read-only mode.
    pub fn start_with_env(env: &[(&str, &str)]) -> Result<Self, String> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jdwp-mcp"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to start jdwp-mcp: {e}"))?;
        let stdin = child.stdin.take().ok_or("server has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("server has no stdout")?);
        let mut server = Self { child, stdin: Some(stdin), stdout, next_id: 1 };
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
        let stdin = self.stdin.as_mut().ok_or("server stdin already closed")?;
        writeln!(stdin, "{req}").map_err(|e| format!("server stdin: {e}"))?;
        stdin.flush().map_err(|e| format!("server flush: {e}"))?;
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

    /// Write one **raw** line to the server's stdin, exactly as given, and do not wait for anything.
    ///
    /// The counterpart to [`request`](Self::request), which can only send well-formed JSON-RPC because it
    /// serialises the request itself. Malformed input is the whole point here: the read loop's parse and
    /// validation arms are the process's front door, and every other test in this harness comes through
    /// it holding a valid request (TEST-9, #25).
    pub fn send_raw(&mut self, line: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("server stdin already closed")?;
        writeln!(stdin, "{line}").map_err(|e| format!("server stdin: {e}"))?;
        stdin.flush().map_err(|e| format!("server flush: {e}"))
    }

    /// Like [`send_raw`](Self::send_raw), but **without** the trailing newline — a stream that ends
    /// mid-message, which is what a client killed halfway through a write leaves behind.
    pub fn send_raw_unterminated(&mut self, text: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("server stdin already closed")?;
        stdin.write_all(text.as_bytes()).map_err(|e| format!("server stdin: {e}"))?;
        stdin.flush().map_err(|e| format!("server flush: {e}"))
    }

    /// Read the **next** line the server writes and parse it as JSON, without skipping anything.
    ///
    /// Unlike [`request`](Self::request), which skips lines until the id matches, this insists on
    /// whatever comes next — which is what makes "the server answered nothing at all" testable: send a
    /// notification, then a request, and assert the next line is the request's reply.
    ///
    /// Blocks until a line arrives or stdout closes.
    pub fn read_reply(&mut self) -> Result<serde_json::Value, String> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(0) => Err("server closed stdout without replying".to_string()),
            Ok(_) => serde_json::from_str(line.trim())
                .map_err(|e| format!("server wrote a non-JSON line ({e}): {line:?}")),
            Err(e) => Err(format!("server stdout: {e}")),
        }
    }

    /// Send a raw line and read the single reply it produces.
    pub fn raw(&mut self, line: &str) -> Result<serde_json::Value, String> {
        self.send_raw(line)?;
        self.read_reply()
    }

    /// Close stdin and wait for the server to exit on its own, returning its status.
    ///
    /// EOF is the *normal* shutdown path (`main.rs`: `Ok(0) => break`) and the one the `Drop` impl below
    /// relies on for coverage counters, so it is worth asserting directly rather than only using it
    /// (TEST-9, #25). `Err` on timeout, which is the failure that matters: a server that hangs on EOF
    /// leaks a process per session.
    pub fn close_stdin_and_wait(&mut self, timeout: Duration) -> Result<std::process::ExitStatus, String> {
        drop(self.stdin.take());
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Ok(None) => return Err(format!("server still running {timeout:?} after EOF on stdin")),
                Err(e) => return Err(format!("waiting for server: {e}")),
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

    /// Poll `debug.get_traces` until the reply contains `needle`, returning the whole reply.
    ///
    /// Traces arrive without suspending anything, so unlike `wait_for_event` there is no hit to
    /// synchronise on — a test either polls or races the debuggee.
    pub fn wait_for_traces(&mut self, needle: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let traces = self.call("debug.get_traces", serde_json::json!({}));
            if traces.contains(needle) {
                return Some(traces);
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        None
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
        // Best-effort: unfreeze the debuggee before walking away, so a dying server can't leave a
        // suspended JVM behind.
        let _ = self.request("tools/call", serde_json::json!({"name": "debug.panic", "arguments": {}}));

        // Shut down by CLOSING STDIN, not by SIGKILL. The server's message loop breaks on EOF
        // (`main.rs`: `Ok(0) => break`), so this is a normal exit — which matters for two reasons:
        //
        //  1. Coverage. Under `cargo llvm-cov` the spawned binary is instrumented, but profile counters
        //     are flushed by an `atexit` handler. SIGKILL skips that, so every one of these processes
        //     wrote no `.profraw` and the integration suite contributed NOTHING to coverage —
        //     `handlers.rs` measured 3.75% while 35 tests were driving it. The number looked like a
        //     plausible low result rather than a broken instrument.
        //  2. It exercises the real shutdown path instead of stepping around it.
        //
        // `kill()` remains the fallback for a server that has wedged, so a hung binary can't hang the
        // suite.
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return, // exited cleanly; counters flushed
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => break,
            }
        }
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
