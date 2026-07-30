// Shared harness for the MCP-level integration tests.
//
// These tests drive the REAL `jdwp-mcp` binary over JSON-RPC on stdio against a real JVM, so they
// cover the server's handler glue — expression resolution, the event pump, deferred-breakpoint
// arming, session bookkeeping — which unit tests can't reach. Each test owns its own probe JVM and
// its own server process, so they can run concurrently.
//
// A JDK is required to compile and run the probes. There is no system JDK on every box (and CI may
// have none at all), so `Jdk::find` returns `Ok(None)` and each test SKIPS rather than fails. Run them
// with:
//
//     scripts/integration-test.sh          # or: cargo test --test mcp_integration -- --ignored
//
// Setting `JAVA_HOME` is a different request, and since TEST-18 (#52) it gets a different answer: it
// names the JDK to test, so if it cannot be honoured the run FAILS instead of quietly testing another
// one. Every run also prints one line saying which JDK it used — see [`JDK_BANNER`].
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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long to wait for a JVM event to show up before calling it a failure. Generous: the probes
/// loop on a ~150ms sleep, but a cold JVM plus class loading can take a second or two.
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(25);

/// The prefix of the one line every run prints to say which JDK it used.
///
/// A constant because it is a contract with `scripts/integration-test.sh`, which greps for it as the
/// third of its green-run-of-nothing guards (TEST-18, #52): a run that never said which JDK it used is a
/// run whose result cannot be attributed to a version, and that is a distinct failure from running
/// nothing at all.
pub const JDK_BANNER: &str = "JDK in use:";

/// Locations of the `java` / `javac` a test should use, and a note of where they were found.
#[derive(Clone)]
pub struct Jdk {
    pub java: PathBuf,
    pub javac: PathBuf,
    /// Which of the three places [`Jdk::find`] looks turned this one up, in words. It is printed and
    /// nothing else reads it — but "`JAVA_HOME`" versus "the snap `JetBrains` runtime" is exactly the
    /// distinction TEST-18 (#52) turned on, so a banner that omitted it would leave the interesting half
    /// unsaid.
    origin: &'static str,
}

impl Jdk {
    /// Find the JDK this run should use. Three outcomes, and the middle one is what TEST-18
    /// ([#52](https://github.com/YgorPerez/java-debugging-mcp/issues/52)) added:
    ///
    ///  * `Ok(Some(jdk))` — use it.
    ///  * `Err(why)` — `JAVA_HOME` is set and does not hold a usable JDK. The caller must **fail**, not
    ///    skip and not search on.
    ///  * `Ok(None)` — `JAVA_HOME` is unset and this machine has no JDK anywhere the search looks. The
    ///    caller SKIPs, which has always been allowed because CI may have no JDK at all.
    ///
    /// **Why the refusal.** `JAVA_HOME` → `PATH` → snap JBR is the right chain for "find me *any* JDK".
    /// It is the wrong chain for the only reason anybody exports `JAVA_HOME` before this suite, which is
    /// to say *which* JDK to test — and those are different questions. On the box where this was found
    /// `JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64` is a JRE with no `javac`, so the old search
    /// discarded it without comment, found no `javac` on `PATH`, and ran the snap `IntelliJ` runtime —
    /// **JDK 25**. Several hours of results were reported as "green on JDK 21" and every one of them was
    /// green on 25. Nothing in the output distinguished that from an unpinned run, because nothing in the
    /// output mentioned a JDK at all; see [`Jdk::banner`] for the other half of the fix.
    ///
    /// Note what deliberately did **not** change: the unset case still searches, in the same order. The
    /// fallback is correct for "any JDK", and requiring every developer to export `JAVA_HOME` would be a
    /// bigger imposition than the bug.
    pub fn find() -> Result<Option<Self>, String> {
        // An EMPTY `JAVA_HOME` counts as unset rather than as a refusal: `JAVA_HOME= cmd` is how a shell
        // spells "no value", and joining `bin` onto "" yields a RELATIVE `bin/javac` resolved against
        // whatever directory cargo happened to run the test in — which is nobody's JDK.
        if let Some(home) = std::env::var_os("JAVA_HOME").filter(|h| !h.is_empty()) {
            let home = PathBuf::from(home);
            let jdk = Self::in_bin(&home.join("bin"), "JAVA_HOME");
            if let Some(shortfall) = jdk.shortfall() {
                return Err(format!(
                    "JAVA_HOME={} is not a usable JDK: {shortfall}.\n\
                     Refusing to fall back to PATH or the snap JetBrains runtime. Exporting JAVA_HOME is \
                     a request for a SPECIFIC JDK, and searching on used to answer it with a different \
                     one in silence — on this very path, a run pinned to JDK 21 ran JDK 25 and said so \
                     nowhere (TEST-18, #52).\n\
                     Point JAVA_HOME at a JDK, or unset it entirely to search for any.",
                    home.display(),
                ));
            }
            return Ok(Some(jdk));
        }
        // No suffix here on purpose: this goes through `CreateProcessW`/`execvp` rather than an
        // existence check, and both resolve the platform's executable extension themselves.
        let on_path = Self { java: PathBuf::from("java"), javac: PathBuf::from("javac"), origin: "PATH" };
        if Command::new(&on_path.javac).arg("-version").output().is_ok_and(|o| o.status.success()) {
            return Ok(Some(on_path));
        }
        // Newest snap revision first, so a stale one doesn't win.
        let mut candidates: Vec<PathBuf> = glob_snap_jbr();
        candidates.sort();
        candidates.reverse();
        Ok(candidates.into_iter().find_map(|bin| {
            let jdk = Self::in_bin(&bin, "the snap JetBrains runtime");
            jdk.is_usable().then_some(jdk)
        }))
    }

    /// The `java`/`javac` pair inside a JDK's `bin`, with the platform's executable suffix.
    ///
    /// The suffix is load-bearing rather than cosmetic: `shortfall` asks the filesystem, and on Windows
    /// the files are `java.exe` and `javac.exe`, so an unsuffixed path never exists. That made
    /// `Jdk::find` return `None` on a machine with a perfectly good JDK at `JAVA_HOME`, and because a
    /// missing JDK skips rather than fails, the entire `--ignored` suite reported `ok` in 0.00s while
    /// running nothing — the same shape as the SIGKILL coverage bug TEST-5 found.
    fn in_bin(bin: &std::path::Path, origin: &'static str) -> Self {
        const EXE: &str = if cfg!(windows) { ".exe" } else { "" };
        Self { java: bin.join(format!("java{EXE}")), javac: bin.join(format!("javac{EXE}")), origin }
    }

    /// What this candidate is missing, in words, or `None` when both tools are there.
    ///
    /// The `java`-but-no-`javac` case gets its own sentence because that difference *is* the incident: a
    /// JRE reads as "Java is installed" to anyone who checks by running `java -version`, and is useless
    /// here, since the probes in `examples/probes` are compiled at test time rather than merely run.
    fn shortfall(&self) -> Option<String> {
        match (self.java.exists(), self.javac.exists()) {
            (true, true) => None,
            (true, false) => Some(format!(
                "there is no javac at {} — only java, so this is a JRE, and the probes in \
                 examples/probes are COMPILED at test time rather than merely run",
                self.javac.display()
            )),
            (false, true) => Some(format!("there is no java at {}", self.java.display())),
            (false, false) => Some(format!(
                "neither java nor javac is in {}",
                self.java.parent().unwrap_or(&self.java).display()
            )),
        }
    }

    fn is_usable(&self) -> bool {
        self.shortfall().is_none()
    }

    /// The one line a run prints to say which JDK it used: version, where the JVM says it lives, and
    /// which of the three places the search found it.
    ///
    /// **This is the half of TEST-18 (#52) that would actually have caught the bug.** The fallthrough was
    /// not invisible because it was subtle; it was invisible because nothing ever said, so a run on the
    /// pinned JDK and a run on some other one produced byte-identical output. A refusal only helps the
    /// person who mis-set `JAVA_HOME`; saying which JDK ran helps every run, including the ones where
    /// `JAVA_HOME` is unset on purpose and the answer is still worth knowing.
    fn banner(&self) -> String {
        format!("{JDK_BANNER} {} at {} (found via {})", self.version(), self.home().display(), self.origin)
    }

    /// `javac -version`'s first line.
    ///
    /// `javac` rather than `java` on two counts: it is the half a JRE lacks, and it is the half that broke
    /// on JDK 11 (TEST-11, #36 — pre-JEP-400 platform-charset source reading). It goes to stdout on JDK 9
    /// and later and to stderr on 8, so both are read rather than guessed at.
    fn version(&self) -> String {
        Command::new(&self.javac)
            .arg("-version")
            .output()
            .ok()
            .and_then(|out| {
                let said = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                said.lines().map(str::trim).find(|l| !l.is_empty()).map(ToString::to_string)
            })
            .unwrap_or_else(|| format!("an unidentified javac ({})", self.javac.display()))
    }

    /// The JDK's feature version — 11, 17, 21 — parsed out of [`version`](Self::version).
    ///
    /// Exists so a test whose *subject* is version-dependent can say so in one line instead of being
    /// version-locked by accident, which is the failure mode CI's three legs keep finding (#36): a test
    /// that passes on 21 and fails on 11 because the JVM legitimately behaves differently there is not a
    /// flake, and it should not be diagnosed as one. `None` when the line cannot be parsed, which callers
    /// should read as "do not gate" rather than "old JDK" — guessing low would silently skip coverage.
    pub fn feature_version(&self) -> Option<u32> {
        // "javac 21.0.1" / "javac 11.0.29" — the feature version is the first dot-separated number.
        self.version().split_whitespace().nth(1)?.split('.').next()?.parse().ok()
    }

    /// Where the JVM says it lives — asked of the JVM rather than inferred from the path it was invoked
    /// through.
    ///
    /// Worth the second process launch because the two cases where inference is worst are the two this
    /// incident ran through. A `PATH` hit is the bare word `javac` and names no directory at all. The snap
    /// runtime is reached through a `current` symlink, so only `java.home` pins the answer to a revision
    /// (`/snap/intellij-idea-ultimate/800/jbr`) rather than to whatever `current` meant that afternoon.
    pub fn home(&self) -> PathBuf {
        Command::new(&self.java)
            .args(["-XshowSettings:properties", "-version"])
            .output()
            .ok()
            .and_then(|out| {
                let said = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                said.lines()
                    .find_map(|l| l.split_once("java.home = "))
                    .map(|(_, home)| PathBuf::from(home.trim()))
            })
            .unwrap_or_else(|| self.javac.clone())
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

    /// Compile a **modified copy** of a checked-in probe into `out_dir` — the build output a hot-reload
    /// test ships to a JVM that is already running the unmodified one (SWAP-1, #58).
    ///
    /// The edit is applied to the probe's real source rather than to a second `.java` kept in step by
    /// hand, because the two versions differing in exactly one intended way is the whole experiment: a
    /// stale copy would make a swap that changed nothing look like a swap that worked, or the reverse.
    /// The modified source is written under `out_dir/src` so the caller can read what was actually
    /// compiled when an assertion fails, and so `out_dir` itself stays a clean class root.
    pub fn compile_probe_variant(
        &self,
        name: &str,
        out_dir: &Path,
        edit: impl FnOnce(String) -> String,
    ) -> Result<PathBuf, String> {
        let original = std::fs::read_to_string(probe_source_path(name))
            .map_err(|e| format!("cannot read the source of probe {name}: {e}"))?;
        let modified = edit(original.clone());
        assert_ne!(
            modified, original,
            "the edit for probe {name} changed nothing, so the variant would be identical to what the \
             JVM is already running and any assertion over it would pass for the wrong reason"
        );
        let src_dir = out_dir.join("src");
        std::fs::create_dir_all(&src_dir).map_err(|e| format!("mkdir {}: {e}", src_dir.display()))?;
        let src = src_dir.join(format!("{name}.java"));
        std::fs::write(&src, modified).map_err(|e| format!("write {}: {e}", src.display()))?;

        let out = Command::new(&self.javac)
            .arg("-g")
            .arg("-encoding")
            .arg("UTF-8")
            .arg("-d")
            .arg(out_dir)
            .arg(&src)
            .output()
            .map_err(|e| format!("failed to run javac: {e}"))?;
        if !out.status.success() {
            return Err(format!("javac {} failed: {}", src.display(), String::from_utf8_lossy(&out.stderr)));
        }
        Ok(out_dir.join(format!("{name}.class")))
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
    entries.flatten().map(|e| e.path().join("jbr/bin")).filter(|p| p.is_dir()).collect()
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

    let bytes =
        std::fs::read(class_file).map_err(|e| format!("cannot read {}: {e}", class_file.display()))?;
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
    let name_length =
        u16::try_from(ATTRIBUTE_NAME.len()).map_err(|_| "attribute name too long".to_string())?;
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
            CONSTANT_UTF8 => 2 + be_u16(bytes, at)?, // a u2 length, then that many bytes
            7 | 8 | 16 | 19 | 20 => 2,               // Class, String, MethodType, Module, Package
            15 => 3,                                 // MethodHandle
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 4, // Integer, Float, the refs, NameAndType, Dynamic
            5 | 6 => 8,                              // Long, Double
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
        .map_or_else(|| panic!("no `{marker}` marker in probe source"), |i| i32::try_from(i).unwrap_or(0) + 1)
}

/// Ask the OS for a free TCP port by binding to port 0 and immediately releasing it.
///
/// Inherently racy — another process could take the port before the JVM binds it. Nothing portable
/// does better, since the JVM must open the port itself, and each test picking a fresh port keeps
/// concurrent tests from colliding, which is the failure that actually happened in practice.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").ok().and_then(|l| l.local_addr().ok()).map_or(0, |a| a.port())
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
                // dt_socket with server=y serves one HANDSHAKED session at a time — it closes its
                // listener for the life of that session and re-opens it when the session ends (measured
                // on JDK 11/21/25 in TEST-20, #55; it is not the "one connection, ever" the comment here
                // used to claim). So connecting lazily — here, not at start — keeps the probe's single
                // slot free until the debugger actually attaches, and keeps the relay from being the
                // thing that shuts the listener.
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

impl Relay {
    /// Shut down every live socket, ending the copy threads, but leave the relay standing.
    ///
    /// The same shutdown [`Drop`] performs, minus the teardown — factored out so a test can sever a
    /// connection deliberately and still hold the relay (and the probe behind it) to inspect afterwards.
    fn sever(&self) {
        if let Ok(v) = self.open.lock() {
            for s in v.iter() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
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

/// What a [`FaultRelay`] does to the debuggee's **unprompted** traffic — its composite event packets.
///
/// Separate from [`Fault`] because events are keyed by nothing. A reply is identified by the command it
/// answers; an event answers no question of ours, so there is no `(set, command)` to match on and the
/// policy has to be positional instead.
#[derive(Clone, Debug)]
pub enum EventFault {
    /// Deliver the first `times` composite events **whose first event is of `kind`** twice.
    ///
    /// This is the debugger-side shape of TEST-23 ([#64](https://github.com/YgorPerez/java-debugging-mcp/issues/64)):
    /// one breakpoint hit that arrives as two buffered events. A real JVM would produce it by having two
    /// armed requests match one location — in which case it sends *one* composite carrying two events
    /// rather than two composites — but the debugger's buffer cannot tell those apart, and duplicating
    /// whole packets needs no knowledge of the VM's id sizes, which per-event surgery would.
    ///
    /// So this reproduces the *observable* faithfully and the *cause* only by analogy, which is the honest
    /// limit of it: it proves what the buffer and the diagnostics do with an extra event. It does not prove
    /// a JVM ever sends one.
    ///
    /// **`kind` is not optional, and the first cut of this learned that the hard way.** It was originally
    /// "the first `n` composite events", which duplicated whichever event happened to arrive first — and
    /// what arrives first is not a property of the test. It passed on JDK 11.0.30 and 25 locally and failed
    /// on CI's Temurin 11.0.31, where the leading event was something else (a `CLASS_PREPARE` from the
    /// deferred-arming watch, or a `VM_START`), so the breakpoint was never duplicated at all. A positional
    /// policy across JVMs is exactly the kind of accidental dependency this suite keeps getting bitten by;
    /// naming the kind makes the target the *event* rather than its position in a stream nobody controls.
    DuplicateKind { kind: u8, times: usize },
}

/// JDWP `eventKind` values, for [`EventFault::DuplicateKind`].
///
/// Only the ones used are named. The full table is in the JDWP spec's `EventKind` constant.
pub const EVENT_KIND_BREAKPOINT: u8 = 2;

/// The `eventKind` of the **first** event in a composite event packet, if it has one.
///
/// The layout is fixed and reachable without knowing the VM's id sizes, which is what makes this cheap:
/// an 11-byte packet header, then `suspendPolicy` (1 byte), then `events` (a 4-byte count), then the first
/// event's `eventKind`. Everything after that is kind-dependent and needs id sizes — which is precisely why
/// this fault duplicates whole packets instead of editing inside one.
fn composite_event_kind(pkt: &[u8]) -> Option<u8> {
    pkt.get(JDWP_HEADER + 1 + 4).copied()
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
    /// Dropping this drops the listener and the sockets with it — and [`sever`](Self::sever) reaches
    /// through it to end a live connection without that teardown.
    relay: Relay,
    /// How many event packets [`EventFault`] has actually duplicated.
    ///
    /// Exposed because without it a test cannot tell **the fault never fired** from **the debugger
    /// coalesced the copies**, and those are opposite conclusions: the first is a broken instrument, the
    /// second a finding about the product. That ambiguity is exactly what made this test's own CI failure
    /// unreadable — it reported `got 1` and left which of the two open.
    duplicated: Arc<std::sync::atomic::AtomicUsize>,
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
        Self::start_with_events(target_port, faults, None)
    }

    /// Listen on a fresh port and make the JVM **refuse** the named commands — the debuggee answers each
    /// with an error and, crucially, *performs nothing*.
    ///
    /// The difference from a [`Fault::Error`] on the same command is the difference between a lie and a
    /// refusal, and it is not cosmetic. A `Fault` rewrites the REPLY, so the command has already landed
    /// and the debuggee has already acted; only the answer is wrong. That is the right instrument for
    /// "the JVM is misreporting" and the wrong one for "the JVM would not do it", which is the state most
    /// error branches are actually written for. FILT-7's failed escalation needs the second: a
    /// `VirtualMachine.Suspend` that errors AND leaves the application running, so a reply claiming the
    /// application is running can be checked against the probe rather than merely read.
    ///
    /// Implemented by dropping the request on the way out and answering it `NOT_IMPLEMENTED` from the
    /// relay, so the debuggee never sees it at all. The first attempt repointed the packet at an unused
    /// command number instead, on the assumption that the JVM would refuse it — `HotSpot`'s debug agent does
    /// not bounds-check the command byte and crashed in native code, which is worth knowing before
    /// anybody tries it again.
    pub fn start_refusing(target_port: u16, refuse: Vec<(u8, u8)>) -> Result<Self, String> {
        Self::start_full(target_port, vec![], None, refuse)
    }

    /// As [`start`](Self::start), and additionally apply `on_events` to the debuggee's composite event
    /// packets — the traffic nobody asked for, which [`Fault`] cannot key on.
    pub fn start_with_events(
        target_port: u16,
        faults: Vec<(u8, u8, Fault)>,
        on_events: Option<EventFault>,
    ) -> Result<Self, String> {
        Self::start_full(target_port, faults, on_events, vec![])
    }

    /// Every policy at once. The three public constructors are named for the question each answers; this
    /// is the one place they are wired.
    fn start_full(
        target_port: u16,
        faults: Vec<(u8, u8, Fault)>,
        on_events: Option<EventFault>,
        refuse: Vec<(u8, u8)>,
    ) -> Result<Self, String> {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let for_relay = Arc::clone(&counter);
        let relay = Relay::start("fault relay", Some(target_port), move |client, server| {
            let Some(server) = server else { return };
            let faults = faults.clone();
            let on_events = on_events.clone();
            let counter = Arc::clone(&for_relay);
            let refuse = refuse.clone();
            // Counted across the whole session rather than per packet, so `times: 1` means "one event
            // only" and a test can stage a second, clean event after it.
            let duplicated = Arc::clone(&counter);
            wire_framed(client, server, refuse, move |seen| {
                let (command, reply) = match seen {
                    // Composite events are *command* packets (set 64) from the debuggee's side. Untouched
                    // unless a policy asks for them: faulting them blindly breaks the event pump rather
                    // than testing it.
                    FromDebuggee::Event(pkt) => {
                        let Some(EventFault::DuplicateKind { kind, times }) = on_events else {
                            return None;
                        };
                        // Matched by kind, not by position: see `DuplicateKind` for the CI failure that
                        // taught this. An event of another kind is forwarded and does not count.
                        if duplicated.load(std::sync::atomic::Ordering::Relaxed) >= times
                            || composite_event_kind(pkt) != Some(kind)
                        {
                            return None;
                        }
                        duplicated.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Two copies in one write. The debugger frames by length, so this is
                        // indistinguishable from the debuggee having sent the event twice.
                        return Some([pkt, pkt].concat());
                    }
                    FromDebuggee::Reply { command, reply, .. } => (command, reply),
                };
                let id = packet_id(reply)?;
                let fault = faults.iter().find(|(s, c, _)| (*s, *c) == command).map(|(_, _, f)| f)?;
                Some(match fault {
                    Fault::Error(code) => reply_packet(id, *code, &[]),
                    Fault::Payload(p) => reply_packet(id, 0, p),
                })
            });
        })?;
        Ok(Self { port: relay.port, relay, duplicated: counter })
    }

    /// Cut the live connection without tearing the relay down, so the debugger sees the debuggee vanish.
    ///
    /// The robust way to manufacture a lost connection: no JVM is killed, so the probe stays available to
    /// be inspected afterwards, and the debugger's socket dies at a moment the test chooses rather than
    /// whenever a killed process happens to be reaped. TEST-24
    /// ([#65](https://github.com/YgorPerez/java-debugging-mcp/issues/65)) is exactly this state.
    pub fn sever(&self) {
        self.relay.sever();
    }

    /// How many event packets the [`EventFault`] has duplicated so far.
    pub fn duplicated(&self) -> usize {
        self.duplicated.load(std::sync::atomic::Ordering::Relaxed)
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
/// original untouched, which is what every packet nobody is interested in gets. The handshake is always
/// forwarded as it arrived — it is fourteen fixed bytes and there is nothing in it to change.
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

/// Requests still waiting for their reply: id → the command it was, and the payload it carried.
///
/// Shared by both directions of a framing proxy — written by the request pump, taken by the reply pump —
/// which is why it is behind an `Arc<Mutex<…>>` rather than owned by either.
type Pending = Arc<Mutex<std::collections::HashMap<u32, (u8, u8, Vec<u8>)>>>;

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

/// JDWP's `NOT_IMPLEMENTED`, which is what a refused command is answered with.
const JDWP_NOT_IMPLEMENTED: u16 = 99;

/// Wire both directions of a **framing** proxy, calling `on_reply` for everything the debuggee sends back.
/// `on_reply` returns a replacement packet, or `None` to forward what arrived.
///
/// `refuse` lists `(command set, command)` pairs the debuggee must never see: the request is dropped on
/// the way out and answered `NOT_IMPLEMENTED` from here, so the JVM does not perform it — see
/// [`FaultRelay::start_refusing`](FaultRelay::start_refusing) for why that is a different instrument from
/// faulting the reply.
///
/// Returns the flag from [`pump_framed`] for the debuggee direction: `true` once the debuggee side has
/// closed, which is the last moment anything can still be recorded.
fn wire_framed(
    client: std::net::TcpStream,
    server: std::net::TcpStream,
    refuse: Vec<(u8, u8)>,
    mut on_reply: impl FnMut(FromDebuggee<'_>) -> Option<Vec<u8>> + Send + 'static,
) -> Arc<std::sync::atomic::AtomicBool> {
    // Which command — and which request payload — each id belongs to, learned from the request direction
    // and read by the reply direction. A reply carries neither, so this map is the only way to key one.
    let pending: Pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (Ok(c_read), Ok(s_read)) = (client.try_clone(), server.try_clone()) else {
        return Arc::new(std::sync::atomic::AtomicBool::new(true));
    };
    // A third handle on the debugger's socket, for answering a refusal without the debuggee's help. Two
    // threads therefore write to it, which is safe here for the reason it would not be in general: a
    // refusal is one 11-byte packet, written with a single `write_all` that becomes a single `send`.
    let mut refusals_to = client.try_clone().ok();

    let outbound = Arc::clone(&pending);
    pump_framed(c_read, server, move |pkt| {
        let (Some(id), Some(flags), Some(set), Some(cmd)) =
            (packet_id(pkt), pkt.get(8).copied(), pkt.get(9).copied(), pkt.get(10).copied())
        else {
            return None;
        };
        if flags & JDWP_REPLY_FLAG != 0 {
            return None;
        }
        if !refuse.contains(&(set, cmd)) {
            if let Ok(mut m) = outbound.lock() {
                m.insert(id, (set, cmd, pkt.get(JDWP_HEADER..).unwrap_or_default().to_vec()));
            }
            return None;
        }
        // Answered here and never forwarded — the whole point is a command the JVM does not perform, so
        // it must not arrive. Deliberately NOT recorded as pending either: no reply will come back from
        // the debuggee for it, and an entry nothing removes would leak for the session.
        //
        // Repointing the packet at an unused command number was the first attempt and is why this is
        // written the long way: `HotSpot`'s debug agent does not bounds-check the command byte, and
        // `(1, 0xFF)` crashed the JVM in native code rather than being refused.
        if let Some(w) = refusals_to.as_mut() {
            let _ = std::io::Write::write_all(w, &reply_packet(id, JDWP_NOT_IMPLEMENTED, &[]));
        }
        // An empty replacement writes nothing, which is how this pump drops a packet.
        Some(Vec::new())
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
    /// The probe's class name, kept only so a failure can say which probe it is talking about — a
    /// timeout that names `EvalProbe` is a different bug report from one that names `LateWorker`.
    name: String,
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
    ///
    /// **Accepting a connection is not the same as running.** The agent binds during JVM startup, before
    /// the main class is loaded, so a probe returned from here may not have executed a line of Java yet.
    /// That is the right thing for the tests that want it — arming a *deferred* breakpoint before its class
    /// exists is what deferred arming is for, and most of the callers here are doing exactly that.
    ///
    /// It is the wrong thing if the first question a test asks is about **loaded state**
    /// (`debug.list_classes`, `debug.list_methods`, `debug.source`): those answer "not loaded" correctly
    /// when the class genuinely is not loaded yet, so losing the race does not fail loudly, it asserts the
    /// wrong finding and blames the tool. Use [`launch_running`](Self::launch_running) for those. TEST-17
    /// (#49) is the incident; `b64d55d` is the two before it.
    pub fn launch(jdk: &Jdk, name: &str) -> Result<Self, String> {
        Self::launch_built_by(jdk, name, None, Jdk::compile_probe)
    }

    /// [`launch`](Self::launch), then block until the probe has demonstrably **run**: a stdout line
    /// matching `ready`.
    ///
    /// Naming the readiness line is left to the caller because only the probe's author knows what running
    /// means for it — most print `tick N`, `EvalProbe` prints `work …` once its static initialiser has
    /// built the objects it hands out, `LateWorker` prints `ready`. A blanket wait inside `launch` would be
    /// worse than no wait at all: it would quietly disarm the deferred-breakpoint tests, which need to arm
    /// *before* the class loads and would then be asserting nothing. So the harness cannot decide readiness,
    /// but it can make asking for it one named call, and it can make failing to get it say **race** rather
    /// than reproduce #46's symptom (TEST-17, #49).
    pub fn launch_running(jdk: &Jdk, name: &str, ready: impl FnMut(&str) -> bool) -> Result<Self, String> {
        let probe = Self::launch(jdk, name)?;
        probe.wait_until_running(EVENT_TIMEOUT, ready)?;
        Ok(probe)
    }

    /// Like [`launch`](Self::launch), but the probe class is not loaded until `delay` after the agent starts
    /// listening — the TEST-17 (#49) race on demand, rather than waiting for a loaded CI runner to hand one
    /// over.
    ///
    /// The JVM is up and answering JDWP the entire time; it simply has not reached the probe yet, which is
    /// precisely what a slow runner looks like from the debugger's side. It is done with a generated wrapper
    /// main class rather than a sleep inside a probe, so no probe carries test scaffolding for this and any
    /// probe can be delayed.
    pub fn launch_delayed(jdk: &Jdk, name: &str, delay: Duration) -> Result<Self, String> {
        Self::launch_built_by(jdk, name, Some(delay), Jdk::compile_probe)
    }

    /// Like [`launch`](Self::launch) but compiled `-g:none` — see [`Jdk::compile_probe_stripped`] for
    /// why exactly one probe wants that, and why it does not weaken the default for the rest.
    pub fn launch_stripped(jdk: &Jdk, name: &str) -> Result<Self, String> {
        Self::launch_built_by(jdk, name, None, Jdk::compile_probe_stripped)
    }

    /// Like [`launch`](Self::launch), but with the probe's checked-in `<name>.smap` installed into
    /// `<name>.class` as a JSR-45 `SourceDebugExtension` before the JVM ever loads it — the state a
    /// JSP-derived servlet is in by the time it reaches the shared 8180 (TEST-15, #40).
    ///
    /// Only the probe's own class is patched. Any other class in the same file is left exactly as
    /// `javac` produced it, which is what gives a test a control compiled in the same breath.
    pub fn launch_with_smap(jdk: &Jdk, name: &str) -> Result<Self, String> {
        Self::launch_built_by(jdk, name, None, |jdk, name, dir| {
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
        start_delay: Option<Duration>,
        build: impl FnOnce(&Jdk, &str, &Path) -> Result<(), String>,
    ) -> Result<Self, String> {
        let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        build(jdk, name, dir.path())?;
        // Either the probe is the main class, or the wrapper is and the probe is loaded late — see
        // [`launch_delayed`](Self::launch_delayed).
        let main_class = match start_delay {
            None => vec![name.to_string()],
            Some(delay) => {
                compile_slow_start(jdk, dir.path())?;
                vec![SLOW_START.to_string(), delay.as_millis().to_string(), name.to_string()]
            }
        };

        let port = free_port();
        // suspend=n so the probe runs immediately; the test attaches while it loops.
        let agent = format!("-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=127.0.0.1:{port}");
        let mut child = Command::new(&jdk.java)
            .arg(agent)
            .args(["-cp", "."])
            .args(&main_class)
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

        let probe = Self { child, stdin, port, name: name.to_string(), lines, new_line, _dir: dir };
        probe.wait_until_listening()?;
        Ok(probe)
    }

    /// Block until the JDWP agent says it is listening, so `debug.attach` can't lose the race with a
    /// still-starting JVM.
    ///
    /// **Readiness is read from the agent's own banner rather than by dialling the port**, and the
    /// difference is not stylistic. This used to prove the port was up with a `TcpStream::connect` whose
    /// result was dropped on the spot, under a comment claiming that `dt_socket` with `server=y` "accepts
    /// ONE connection then stops listening, so that probe connection is the one the server will use". Both
    /// halves were wrong, and TEST-20 (#55) measured it: connect, close, connect again and complete a
    /// handshake works on JDK 11, 21 and 25 alike, because the agent only stops listening once a debugger
    /// has finished a **handshake**, and starts again when that session ends. A connection that never
    /// speaks JDWP costs it nothing.
    ///
    /// What it did cost was the truth of every probe's captured output. A connect that closes without
    /// handshaking makes the agent print
    ///
    /// ```text
    /// Debugger failed to attach: handshake failed - connection prematurally closed
    /// ```
    ///
    /// to the JVM's stderr — the JDK's own typo — and `pump` folds stderr into the same buffer the tests
    /// assert over and print on failure. So every probe in the suite carried a line saying the debugger
    /// had failed to attach, moments before it attached perfectly well. #55 was filed on that line, read
    /// from `ChurnProbe`'s log as evidence of a real attach failure. Readiness that says nothing to the
    /// agent leaves the log honest.
    ///
    /// The banner is printed by the agent immediately after `listen()` returns and before any client can
    /// be accepted, so it is if anything *earlier* than the old 100ms connect poll could notice. It is
    /// suppressed by the agent's `quiet=y` option, which is exactly why [`launch_built_by`] owns the agent
    /// argument and does not pass it.
    ///
    /// **That earliness cost a red `main` the same day, and the lesson is worth more than the incident.**
    /// The old connect poll slept 100ms between attempts, so it returned tens of milliseconds after the
    /// port came up and handed every test slack it had never asked for. Two tests were living on it:
    /// they armed a watchpoint — which [cannot be deferred](crate::handlers) — against a class the JVM had
    /// not loaded yet, and only won because of the delay. They now say what they need via
    /// [`launch_running`](Self::launch_running). If a test of yours starts failing with *"is not loaded
    /// yet"*, this is why, and the fix is to state the dependency rather than to slow readiness back down:
    /// a timing accident that makes a test pass is not the test passing.
    ///
    /// One thing it can do that a connect cannot: a connect proves only that **something** answers on that
    /// port, and [`free_port`] documents that something else may have taken it before the JVM got there.
    /// This reads the port out of *this* JVM's own banner, so a probe whose agent lost that race now waits
    /// out the timeout and reports what the JVM said — including a bind failure — instead of pronouncing a
    /// stranger's listener ready and handing `debug.attach` the wrong JVM.
    fn wait_until_listening(&self) -> Result<(), String> {
        let started = Instant::now();
        // Deliberately not matched against the whole line: JDK 11 prints the bare port where later ones
        // may print `host:port`, and the transport name is the agent's to spell.
        let port = self.port.to_string();
        let banner = |l: &str| l.starts_with("Listening for transport ") && l.trim_end().ends_with(&port);
        if self.wait_for_line(Self::PROBE_LISTEN_TIMEOUT, banner).is_some() {
            return Ok(());
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
            format!("it printed:\n  {}", tail.iter().map(|l| l.as_str()).collect::<Vec<_>>().join("\n  "))
        };
        Err(format!(
            "probe never announced a JDWP listener on port {} within {:?} (waited {:?}) — {said}\n\
             Expected a line like `Listening for transport dt_socket at address: {}` from the agent \
             itself.\n\
             If it printed nothing, the JVM is probably just slow to start rather than broken: on \
             Windows a first run after a JDK is installed or updated can spend longer than this being \
             scanned by Defender, and the same probe then launches in ~1s once warm.",
            self.port,
            Self::PROBE_LISTEN_TIMEOUT,
            started.elapsed(),
            self.port,
        ))
    }

    /// Block until the probe prints a line matching `ready` — until it is *running*, not merely listening.
    ///
    /// The other half of [`wait_until_listening`](Self::wait_until_listening), and the reason it is separate
    /// is that only the caller can say what running means for a given probe. What this adds over a bare
    /// [`wait_for_line`](Self::wait_for_line) is the failure text: a probe that never runs has to be
    /// reported as a **race in the test**, because the alternative is what actually happened in TEST-17
    /// (#49) — the discovery tool answers "not loaded", correctly, the assertion fails, and the message
    /// reads exactly like the wrong-answer bug #46 was about, in a file the reader has no reason to open.
    pub fn wait_until_running(
        &self,
        timeout: Duration,
        ready: impl FnMut(&str) -> bool,
    ) -> Result<String, String> {
        self.wait_for_line(timeout, ready).ok_or_else(|| {
            format!(
                "{name} accepted a JDWP connection but never printed a readiness line within {timeout:?} \
                 — it is listening, not running.\n\
                 This is a RACE in the test, not a wrong answer from the debugger: the JDWP agent binds \
                 before the main class is loaded, so anything asked now about loaded state \
                 (debug.list_classes, debug.list_methods, debug.source) is correctly answered \"not \
                 loaded\". See TEST-17 (#49) — and do not go looking for #46's wrong-answer bug, which is \
                 what this looks like from the assertion's side.\n\
                 What {name} printed: {output:?}",
                name = self.name,
                output = self.output(),
            )
        })
    }

    /// Send a line to the probe's stdin (probes that wait for a cue to do something).
    pub fn send_line(&mut self, line: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("probe stdin already closed")?;
        writeln!(stdin, "{line}").map_err(|e| format!("probe stdin write: {e}"))?;
        stdin.flush().map_err(|e| format!("probe stdin flush: {e}"))
    }

    /// Attach `server` to **this** probe, and if that fails, say what the probe itself said.
    ///
    /// TEST-21 ([#56](https://github.com/YgorPerez/java-debugging-mcp/issues/56)), first acceptance
    /// criterion: both sightings of `attach to port N failed: Connection refused` are after-the-fact test
    /// output, and *"nobody has yet seen what the probe's own log said at the moment of refusal"*.
    /// `Server::attach` cannot say — it is handed a port and no probe. This is.
    ///
    /// #55 narrowed the causes to two, and the evidence separates them:
    ///
    /// - **A live handshaked session holds the port**, which provably refuses a second attach. Then
    ///   *something is listening*, and the raw connect below succeeds.
    /// - **[`free_port`]'s documented race**: a stranger took the port and the JVM never bound it. Then
    ///   nothing of ours is listening, the connect fails too, and the probe's log — which since #55 comes
    ///   from *this* JVM and names *this* port — should carry a bind failure rather than the
    ///   `Listening for transport` banner.
    ///
    /// The raw connect is deliberate and is only done on the failure path. #55 measured that a connection
    /// which never handshakes costs the agent nothing, so it cannot make a bad situation worse; what it
    /// does do is make the JVM print its `handshake failed` line, which is why the log is captured
    /// *before* the connect and why the message says the last line may be this probe's own doing.
    pub fn attach(&mut self, server: &mut Server) -> String {
        let out = server.call("debug.attach", serde_json::json!({"host": "127.0.0.1", "port": self.port}));
        if out.contains("Connected") {
            return out;
        }
        let diagnosis = self.diagnose_refusal(&out);
        panic!("{diagnosis}");
    }

    /// Why an attach to this probe might have failed, as text.
    ///
    /// Separate from [`attach`](Self::attach) so the worlds it distinguishes can be *tested* rather than
    /// hoped for — a diagnosis nobody has exercised is as trustworthy as the `Connection refused` it
    /// replaces. See the `a_refused_attach_*` tests, which manufacture each world and assert this names
    /// it. Takes `&mut self` for one reason: [`Child::try_wait`], below.
    ///
    /// **Three readings, not two — and the first version of this got the tree backwards.** #56 enumerated
    /// a live session and [`free_port`]'s race, both of which assume a JVM that is still running. A third
    /// is at least as likely under CI's parallelism and was not on the list: **the JVM is gone**, because
    /// its `main` returned, it threw, or something killed it.
    ///
    /// What the tree keyed on was whether anything was listening — *"listening ⇒ the port is taken by a
    /// live handshaked session"*. Measured (`a_refused_attach_*` below), both worlds report **nothing
    /// listening**, so that branch was unreachable for the mechanism it named. It follows from #55's own
    /// finding, one step further on than #55 took it: the agent serves one handshaked session at a time and
    /// **closes its listener for that session's life**. So "nothing listening" is the *expected* signature
    /// of a live session, not evidence of a failure to bind — and the old wording read it as the latter,
    /// sending the reader after a bind error while the banner sat in the log contradicting it.
    ///
    /// What actually separates them is whether the JVM is still running, and then whether it ever
    /// announced the port. Both are cheap, and this returns the verdict rather than the tree, because a
    /// reader who has to evaluate three branches by hand is a reader who will pick the wrong one.
    pub fn diagnose_refusal(&mut self, out: &str) -> String {
        // Read before connecting: the connect below makes the agent print its own `handshake failed`
        // line, and #55 was filed on mistaking exactly that for evidence.
        let log = self.output();
        let announced = log.iter().any(|l| l.contains("Listening for transport"));
        let exit = self.child.try_wait();
        let listening = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_millis(500),
        )
        .is_ok();

        let verdict = refusal_verdict(
            match &exit {
                Ok(None) => JvmState::Alive,
                Ok(Some(status)) => JvmState::Exited(status.to_string()),
                Err(e) => JvmState::Unknown(e.to_string()),
            },
            listening,
            announced,
        );

        format!(
            "attach to {} on port {} failed: {out}\n  \
             verdict: {verdict}\n  \
             the facts it was read from — JVM: {}; listening on the port: {}; announced this port: {}\n  \
             The probe's last 12 lines, as of BEFORE this diagnosis connected to it (#55 made the \
             `Listening for transport … at address:` banner come from this JVM and name this port, so \
             its absence is itself the finding):\n{}",
            self.name,
            self.port,
            match &exit {
                Ok(None) => "alive".to_string(),
                Ok(Some(status)) => format!("exited with {status}"),
                Err(e) => format!("try_wait failed: {e}"),
            },
            if listening { "yes" } else { "no" },
            if announced { "yes" } else { "no" },
            log.iter().rev().take(12).rev().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n"),
        )
    }

    /// Stop the probe's JVM and wait for it to be gone, so a test can manufacture the "JVM died" world.
    ///
    /// Returns once the process has been reaped *and* the port stops accepting, because a killed JVM's
    /// listener outlives it by a moment and a test that raced that would be the flake it is investigating.
    pub fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let dialled = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], self.port)),
                Duration::from_millis(200),
            );
            if dialled.is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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

/// The generated main class [`Probe::launch_delayed`] runs instead of the probe.
const SLOW_START: &str = "SlowStart";

/// Write and compile [`SLOW_START`] into `dir`: a main class that sleeps, then loads and runs the probe.
///
/// Two properties are the point. It sleeps *before* touching the probe class, so the probe is genuinely not
/// loaded rather than loaded-and-idle — `debug.list_classes` cannot see it, which is the state TEST-17 (#49)
/// is about. And it sleeps in Java rather than delaying the launch from Rust, so the JVM is fully up and
/// answering JDWP throughout: a test can attach, ask, and get the same "not loaded" a slow runner produces.
fn compile_slow_start(jdk: &Jdk, dir: &Path) -> Result<(), String> {
    let src = dir.join(format!("{SLOW_START}.java"));
    std::fs::write(
        &src,
        format!(
            "public class {SLOW_START} {{\n    \
                 public static void main(String[] args) throws Exception {{\n        \
                     Thread.sleep(Long.parseLong(args[0]));\n        \
                     Class.forName(args[1]).getMethod(\"main\", String[].class)\n            \
                         .invoke(null, (Object) new String[0]);\n    \
                 }}\n}}\n"
        ),
    )
    .map_err(|e| format!("cannot write {}: {e}", src.display()))?;
    let out = Command::new(&jdk.javac)
        .args(["-g", "-encoding", "UTF-8", "-d"])
        .arg(dir)
        .arg(&src)
        .output()
        .map_err(|e| format!("failed to run javac for {SLOW_START}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!("javac failed for {SLOW_START}: {}", String::from_utf8_lossy(&out.stderr)))
}

/// Drain one of the probe's output streams into `sink`, notifying `tx` per line.
/// How much of the server's stderr to keep. Enough to cover the exchange a failure happened during,
/// bounded because a whole test's worth of `jdwp_mcp=info` is thousands of lines nobody reads.
const SERVER_LOG_TAIL: usize = 60;

/// What [`Probe::diagnose_refusal`] could learn about the probe's process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JvmState {
    Alive,
    /// Rendered rather than a `std::process::ExitStatus`, because an `ExitStatus` cannot be constructed
    /// portably and a verdict that cannot be unit-tested is the thing this refactor exists to prevent.
    Exited(String),
    Unknown(String),
}

/// Which world a refused attach is in, from the three facts that distinguish them.
///
/// TEST-21 ([#56](https://github.com/YgorPerez/java-debugging-mcp/issues/56)). Pulled out of
/// [`Probe::diagnose_refusal`] because **two of these four verdicts could not be reached from any state a
/// test can build with a real JVM**: a stranger holding the port needs a `free_port` race to be won on
/// purpose, and a JVM that never bound its port needs the JVM to lose one. Both were shipped unverified,
/// which is precisely the position the first version of this diagnosis was in when it turned out to have
/// its decision tree backwards.
///
/// The split is deliberate: this decides, and is unit-tested over all four worlds with no JVM at all; the
/// two `a_refused_attach_*` integration tests prove that a real refusal produces the right *inputs*. A
/// simulation is only as good as the seam it plugs into, and this is the seam.
pub fn refusal_verdict(jvm: JvmState, listening: bool, announced: bool) -> String {
    match (jvm, listening, announced) {
        (JvmState::Unknown(e), _, _) => {
            format!("UNDETERMINED — could not read the JVM's status ({e}); the facts below are all there is.")
        }
        (JvmState::Exited(status), _, _) => format!(
            "THE PROBE JVM IS GONE — it exited with {status}, and the port went with it. This is not a \
             port race and `free_port` explains none of it; find out why a JVM that had announced \
             itself stopped running."
        ),
        (JvmState::Alive, true, _) => "SOMETHING ELSE HOLDS THE PORT — something is listening, and it is \
             not this probe's agent, which stops listening the moment a debugger completes a handshake. A \
             stranger won free_port's race after this JVM bound and released it, or never let it bind."
            .to_string(),
        (JvmState::Alive, false, true) => "THE SESSION IS ALREADY TAKEN — the JVM is alive and its banner \
             names this port, so it did bind it. A live handshaked session refuses a second attach *and* \
             closes the listener, so \"nothing listening\" is this world's signature rather than a fault. \
             Find what is already attached: a leaked session from an earlier test, a `Relay`, or a \
             debugger the harness believes it disconnected."
            .to_string(),
        (JvmState::Alive, false, false) => "THE PORT WAS NEVER BOUND — the JVM is alive but never printed \
             the `Listening for transport` banner for this port, which is `free_port`'s documented TOCTOU. \
             Its log should carry the bind failure; nothing portable removes this race, so the remedy is \
             that this message exists."
            .to_string(),
    }
}

/// Which of two worlds a missing debuggee effect is in, decided by whether the debuggee ran at all.
///
/// TEST-16 ([#45](https://github.com/YgorPerez/java-debugging-mcp/issues/45)). *"The caller never observed
/// the forced value"* reads as an accusation against `debug.force_return`, and **a VM that was never
/// resumed is silent in exactly the same way** — a debugger bug and a harness bug wearing one message. The
/// discriminator is the debuggee's own output across the resume: a probe that printed nothing never ran.
///
/// Extracted from the assertion it was written inside so it can be *tested*. It was originally verified by
/// fault injection at a keyboard, which proves it worked once and nothing thereafter — and this repo has
/// already shipped one bug behind a test that asserted the presence of an aside rather than the verdict.
pub const fn resume_verdict(printed_before: usize, printed_after: usize) -> &'static str {
    if printed_after == printed_before {
        "it never ran again — read this as a resume/liveness failure (or a dead probe), NOT as \
         force_return returning the wrong value"
    } else {
        "it DID run and still never produced the forced value — force_return reported success \
         without changing what the caller received, which is exactly what this test exists to catch"
    }
}

/// Drain a stream for its whole life, keeping only the last `keep` lines.
///
/// Draining is the requirement, not the tail: an unread pipe fills at 64 KiB and blocks the process
/// writing to it. The bound is just so what is kept stays readable in a panic message.
fn pump_tail<R: std::io::Read + Send + 'static>(
    stream: R,
    sink: Arc<Mutex<std::collections::VecDeque<String>>>,
    keep: usize,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Ok(mut v) = sink.lock() {
                if v.len() == keep {
                    v.pop_front();
                }
                v.push_back(line);
            }
        }
    });
}

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
    /// `Option` so [`close_stdin_and_wait`](Server::close_stdin_and_wait) can hand it to a draining
    /// thread; `None` afterwards, and the lines it read are in `drained`.
    stdout: Option<BufReader<ChildStdout>>,
    /// Lines read by that drain and not yet consumed by [`read_reply`](Server::read_reply), oldest
    /// first — so closing stdin does not swallow the replies a test still means to assert on.
    drained: std::collections::VecDeque<String>,
    /// The server's own stderr, most recent [`SERVER_LOG_TAIL`] lines. Diagnostic only: no assertion
    /// reads it, and it is printed by the failure paths that used to report a symptom with no context.
    log: Arc<Mutex<std::collections::VecDeque<String>>>,
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
            // Captured rather than discarded. This was `Stdio::null()`, which threw away the only
            // account of what the server thought was happening — so a test that failed because the
            // debuggee's connection died reported the symptom and nothing else. Piped output *must* be
            // drained (a full pipe blocks the writer, which is the deadlock `close_stdin_and_wait`
            // documents), so `pump_tail` reads it on a thread for as long as the server lives.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start jdwp-mcp: {e}"))?;
        let stdin = child.stdin.take().ok_or("server has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("server has no stdout")?);
        let stderr = child.stderr.take().ok_or("server has no stderr")?;
        let log = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        pump_tail(stderr, Arc::clone(&log), SERVER_LOG_TAIL);
        let mut server = Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            drained: std::collections::VecDeque::new(),
            log,
            next_id: 1,
        };
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
        loop {
            let line = self.next_line()?;
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
        let line = self.next_line()?;
        serde_json::from_str(line.trim()).map_err(|e| format!("server wrote a non-JSON line ({e}): {line:?}"))
    }

    /// The next line the server wrote, from the drain buffer first and the pipe second.
    ///
    /// The buffer exists because [`close_stdin_and_wait`](Self::close_stdin_and_wait) has to read the
    /// pipe to let the server exit, and the replies it reads while doing so are still the test's.
    fn next_line(&mut self) -> Result<String, String> {
        if let Some(line) = self.drained.pop_front() {
            return Ok(line);
        }
        let stdout = self.stdout.as_mut().ok_or("server closed stdout without replying")?;
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(0) => Err("server closed stdout without replying".to_string()),
            Ok(_) => Ok(line),
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
    ///
    /// **stdout is drained while waiting, and that is load-bearing rather than tidy.** A pipe holds
    /// 64 KiB on Linux; a writer that fills it blocks until somebody reads. Waiting for exit without
    /// reading therefore deadlocks the moment a pending reply is larger than the buffer — and one
    /// silently became so: adding three tools took `tools/list` from 58,408 bytes to 66,334, and
    /// `a_final_request_without_a_trailing_newline_is_answered_at_eof` began failing with *"server still
    /// running 10s after EOF"*, which reads as a shutdown bug in the server and was a full pipe in the
    /// harness. The lines read here are kept for [`read_reply`](Self::read_reply), so a test that closes
    /// stdin and then asserts on the reply still gets it.
    ///
    /// The drain runs on its own thread because a blocking read cannot be given a deadline, and the
    /// deadline is the whole point of this function: on timeout the thread is abandoned rather than
    /// joined, so a genuinely hung server still fails in `timeout` rather than hanging the suite.
    pub fn close_stdin_and_wait(&mut self, timeout: Duration) -> Result<std::process::ExitStatus, String> {
        drop(self.stdin.take());
        let (tx, rx) = channel();
        if let Some(mut stdout) = self.stdout.take() {
            std::thread::spawn(move || loop {
                let mut line = String::new();
                match stdout.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            });
        }
        let deadline = Instant::now() + timeout;
        let outcome = loop {
            while let Ok(line) = rx.try_recv() {
                self.drained.push_back(line);
            }
            match self.child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Ok(None) => break Err(format!("server still running {timeout:?} after EOF on stdin")),
                Err(e) => break Err(format!("waiting for server: {e}")),
            }
        };
        // The process exiting does not mean the drain has caught up, and the reply a caller is about to
        // assert on may be the last thing through the pipe.
        while let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            self.drained.push_back(line);
        }
        outcome
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
            Err(e) => format!("<transport error> {e}{}", self.log_tail()),
        }
    }

    /// The server's recent stderr, formatted for a panic message, or an empty string if it said nothing.
    ///
    /// Appended to failure text rather than asserted on. A test that depended on log wording would break
    /// every time a log line was reworded, which is how diagnostics turn into maintenance.
    pub fn log_tail(&self) -> String {
        let lines = match self.log.lock() {
            Ok(l) => l.iter().cloned().collect::<Vec<_>>(),
            Err(_) => return String::new(),
        };
        if lines.is_empty() {
            return String::new();
        }
        format!("\n--- the server's last {} stderr line(s) ---\n{}", lines.len(), lines.join("\n"))
    }

    /// Attach to a probe, panicking with the server's own message if it fails.
    pub fn attach(&mut self, port: u16) -> String {
        let out = self.call("debug.attach", serde_json::json!({"host": "127.0.0.1", "port": port}));
        assert!(out.contains("Connected"), "attach to port {port} failed: {out}{}", self.log_tail());
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

/// The JDK for the whole run, resolved once and announced once.
///
/// A `OnceLock` rather than a search per test, for two reasons that pull the same way. Which JDK ran is a
/// property of the *run*, so printing it once per test would turn the one line that matters into
/// wallpaper. And resolving now costs two extra process launches — a version and a home — which is
/// nothing once and silly sixty-five times.
fn resolved_jdk() -> &'static Result<Option<Jdk>, String> {
    static RESOLVED: OnceLock<Result<Option<Jdk>, String>> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        let found = Jdk::find();
        // stdout rather than stderr, in both arms: `scripts/integration-test.sh` tees stdout and greps
        // the log for its guards, and the banner is now one of the things it looks for.
        match &found {
            Ok(Some(jdk)) => println!("{}", jdk.banner()),
            // Nothing to announce; the per-test SKIP line below is the report, and the script fails on it.
            Ok(None) => {}
            Err(why) => println!("error: {why}"),
        }
        found
    })
}

/// Skip-with-a-reason guard. Returns the JDK, or `None` after printing why the test is skipped.
///
/// A missing JDK must not fail the suite — CI may have none — but a silent pass would hide that
/// nothing ran, so it says so on stdout (visible with `cargo test -- --nocapture`).
///
/// An **unusable `JAVA_HOME`** is the one case that panics instead. A skip there would be the original
/// bug wearing a different hat: the run asked for a specific JDK, the request cannot be honoured, and any
/// outcome other than failing lets the suite report a version it never tested (TEST-18, #52). The full
/// explanation was printed once by [`resolved_jdk`]; the line here is what shows up against each failing
/// test, so it names the path and the shortfall on its own rather than pointing at something above.
pub fn jdk_or_skip(test: &str) -> Option<Jdk> {
    match resolved_jdk() {
        Ok(Some(jdk)) => Some(jdk.clone()),
        Ok(None) => {
            println!("SKIP {test}: no JDK found (set JAVA_HOME or put javac on PATH)");
            None
        }
        Err(why) => panic!("{}", why.lines().next().unwrap_or(why)),
    }
}

/// Assert `got` contains every string in `wants`, with a message naming the ones it doesn't.
pub fn assert_contains_all(label: &str, got: &str, wants: &[&str]) {
    let missing: Vec<&str> = wants.iter().copied().filter(|w| !got.contains(w)).collect();
    assert!(missing.is_empty(), "{label}: missing {missing:?}\n  got: {got}");
}
