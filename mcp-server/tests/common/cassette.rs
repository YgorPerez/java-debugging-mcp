// Cassettes: a recorded JDWP session, and a server that answers from one with no JVM behind it.
//
// TEST-12 ([#37](https://github.com/YgorPerez/java-debugging-mcp/issues/37)). TEST-8's residue was a single
// sentence — *run one dump against the real 8180 and read the numbers* — and the trouble with it is that
// the evidence evaporates when the session ends. The next question about the real instance needs another
// visit, by someone with access, on a box with a JDK.
//
// A cassette makes one visit permanent. Every request/reply pair the debugger and the debuggee exchange is
// written to a file; a replay server then serves that file on a port the debugger attaches to exactly as it
// would attach to a JVM. What comes out is a test that needs no JVM, no JDK and no access, and is therefore
// not `#[ignore]`d — and a fixture that can be *edited* into shapes nothing on this box can produce: a JVM
// answering `JDWP 1.5`, a truncated reply, a thousand-thread pool, a stack four hundred frames deep.
//
// Three rules the format follows, and the reasons are in ADR-0014:
//
//  1. **Keyed by command + request payload, not arrival order.** The event pump reads the same socket the
//     commands go down, so strict ordering is a property of the machine's scheduler as much as of the
//     debugger. Keying by what was asked survives that.
//  2. **A miss is LOUD.** An unmatched request gets no reply at all — the connection is dropped, the command
//     is named on stderr and remembered, and `ReplayServer`'s own `Drop` fails the test if nobody looked. A
//     replay that quietly answered `INVALID_OBJECT` would make every test using it green and worthless,
//     which is this repo's recurring failure mode rather than a hypothetical one.
//  3. **Readable and hand-editable.** JSON, one exchange per object, payloads as hex broken into 32-byte
//     lines, and each exchange labelled with the JDWP command name it carries.
//
// **Events are not replayed.** A composite event answers no request, so it has no key; replaying one needs a
// timer or a cue and neither is honest about when the debuggee would really have spoken. The recorder counts
// them and writes the count into the cassette, and both the recorder and `Cassette::load` say so out loud
// when it is non-zero. That is the deliberate first-cut limit the issue allowed, stated rather than
// half-supported: a cassette of a breakpoint session records the commands faithfully and will never fire the
// breakpoint.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    packet_id, read_frames, reply_packet, wire_framed, Frame, FromDebuggee, Relay, JDWP_HANDSHAKE,
    JDWP_HEADER, JDWP_REPLY_FLAG,
};

/// One request and the reply it got, which is the whole of what a cassette stores.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub set: u8,
    pub command: u8,
    pub request: Vec<u8>,
    pub error: u16,
    pub reply: Vec<u8>,
}

/// A recorded JDWP session, replayable and hand-editable.
#[derive(Clone, Debug, Default)]
pub struct Cassette {
    /// What this recorded. Free text, written into the file so a reader knows what they are holding.
    pub title: String,
    /// Where it came from — the probe, and the JDK that ran it. A cassette is a snapshot of one debuggee
    /// on one JVM, and saying which is the difference between a fixture and a mystery.
    pub recorded_from: String,
    /// Anything a human wants the next reader to know, including any edits made by hand.
    pub note: String,
    /// Composite events seen while recording, and NOT stored. Non-zero means this cassette is a partial
    /// record of a session that had the debuggee speaking unprompted — see the module header.
    pub events_seen: usize,
    exchanges: Vec<Exchange>,
    /// Per-connection bookkeeping so a request recorded twice is answered twice, in order. Not written to
    /// the file; reset by [`rewind`](Self::rewind).
    served: Vec<bool>,
}

impl Cassette {
    /// How many exchanges are on the tape.
    pub fn len(&self) -> usize {
        self.exchanges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exchanges.is_empty()
    }

    /// Every exchange, in the order recorded — for a test that wants to assert on the tape itself.
    pub fn exchanges(&self) -> &[Exchange] {
        &self.exchanges
    }

    /// Forget which exchanges have been served, so this tape can answer a fresh connection.
    pub fn rewind(&mut self) {
        self.served = vec![false; self.exchanges.len()];
    }

    /// The reply to one request, or `None` if this cassette never saw it.
    ///
    /// **First unused match, then the last match.** Taking the first *unused* one is what lets a request
    /// asked twice get its two recorded answers in order — `VirtualMachine.AllThreads` before and after a
    /// pool grew is the same key and two different worlds, and order within one key is the only thing that
    /// can tell them apart. Falling back to the last match once they run out is not the same compromise: a
    /// caller polling one more time than the recording did is asking about a world the cassette does
    /// describe, and failing it would make every replay depend on the debugger's retry counts matching to
    /// the call. A key that was never recorded at all is the case that must fail, and it does.
    pub fn answer(&mut self, set: u8, command: u8, request: &[u8]) -> Option<(u16, Vec<u8>)> {
        if self.served.len() != self.exchanges.len() {
            self.rewind();
        }
        let matches = |e: &Exchange| e.set == set && e.command == command && e.request.as_slice() == request;
        if let Some(i) = (0..self.exchanges.len())
            .find(|&i| !self.served.get(i).copied().unwrap_or(true) && matches(&self.exchanges[i]))
        {
            if let Some(flag) = self.served.get_mut(i) {
                *flag = true;
            }
            let e = self.exchanges.get(i)?;
            return Some((e.error, e.reply.clone()));
        }
        let last = self.exchanges.iter().rev().find(|e| matches(e))?;
        Some((last.error, last.reply.clone()))
    }

    /// Read a cassette from disk.
    ///
    /// # Errors
    /// Returns the reason as text — a missing file, JSON that does not parse, a field of the wrong shape,
    /// or a payload that is not hex. Hand editing is a supported way to make a cassette, so every one of
    /// those is a mistake a human will make, and each says which exchange it is about.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read cassette {}: {e}", path.display()))?;
        let tape = Self::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if tape.events_seen > 0 {
            eprintln!(
                "note: cassette {} was recorded from a session with {} composite event(s), which are NOT \
                 replayed — nothing in it will fire a breakpoint (see common/cassette.rs)",
                path.display(),
                tape.events_seen
            );
        }
        Ok(tape)
    }

    /// Write a cassette to disk, creating the directory if it is not there.
    ///
    /// # Errors
    /// Returns the reason as text if the directory cannot be created or the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        std::fs::write(path, self.to_json())
            .map_err(|e| format!("cannot write cassette {}: {e}", path.display()))
    }

    /// The cassette as the JSON that gets checked in.
    ///
    /// Written by hand rather than through `serde_json::to_string_pretty` for one reason: field order. A
    /// `serde_json::Map` is a `BTreeMap`, so the pretty printer would put `cmd`, `command`, `error`,
    /// `reply`, `request`, `set` in that order — alphabetical, and useless to read. Here the label comes
    /// first, then what was asked, then what came back, which is the order a person reads an exchange in.
    /// Parsing goes back through `serde_json` and does not care about order at all, so a hand edit that
    /// moves a field is still a valid cassette.
    pub fn to_json(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        out.push_str("{\n");
        let _ = writeln!(out, "  \"cassette\": {},", json_string(&self.title));
        let _ = writeln!(out, "  \"recorded_from\": {},", json_string(&self.recorded_from));
        let _ = writeln!(out, "  \"note\": {},", json_string(&self.note));
        let _ = writeln!(out, "  \"events_seen\": {},", self.events_seen);
        out.push_str("  \"exchanges\": [\n");
        for (i, e) in self.exchanges.iter().enumerate() {
            let comma = if i + 1 == self.exchanges.len() { "" } else { "," };
            out.push_str("    {\n");
            let _ = writeln!(out, "      \"command\": {},", json_string(&command_name(e.set, e.command)));
            let _ = writeln!(out, "      \"set\": {}, \"cmd\": {},", e.set, e.command);
            let _ = writeln!(out, "      \"request\": {},", hex_field(&e.request, 6));
            let _ = writeln!(out, "      \"error\": {},", e.error);
            let _ = writeln!(out, "      \"reply\": {}", hex_field(&e.reply, 6));
            let _ = writeln!(out, "    }}{comma}");
        }
        out.push_str("  ]\n}\n");
        out
    }

    /// Parse a cassette. Payloads may be one hex string or an array of them, concatenated — the writer
    /// emits an array once a payload passes a line's worth, because a four-kilobyte stack reply on one
    /// line is technically editable and practically not.
    ///
    /// # Errors
    /// Returns the reason as text, naming the exchange index whenever it can.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        let raw = v
            .get("exchanges")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "no `exchanges` array".to_string())?;
        let mut exchanges = Vec::with_capacity(raw.len());
        for (i, e) in raw.iter().enumerate() {
            let field = |name: &str| -> Result<u64, String> {
                e.get(name)
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| format!("exchange {i} has no numeric `{name}`"))
            };
            let set =
                u8::try_from(field("set")?).map_err(|_| format!("exchange {i}: `set` is not a byte"))?;
            let command =
                u8::try_from(field("cmd")?).map_err(|_| format!("exchange {i}: `cmd` is not a byte"))?;
            let error = u16::try_from(field("error")?)
                .map_err(|_| format!("exchange {i}: `error` does not fit a u2"))?;
            exchanges.push(Exchange {
                set,
                command,
                request: hex_value(e.get("request")).map_err(|m| format!("exchange {i} `request`: {m}"))?,
                error,
                reply: hex_value(e.get("reply")).map_err(|m| format!("exchange {i} `reply`: {m}"))?,
            });
        }
        let text_field =
            |name: &str| v.get(name).and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
        let served = vec![false; exchanges.len()];
        Ok(Self {
            title: text_field("cassette"),
            recorded_from: text_field("recorded_from"),
            note: text_field("note"),
            events_seen: usize::try_from(
                v.get("events_seen").and_then(serde_json::Value::as_u64).unwrap_or(0),
            )
            .unwrap_or(0),
            exchanges,
            served,
        })
    }
}

/// A proxy that writes down everything that passes through it.
///
/// Recording is [`FaultRelay`](super::FaultRelay)'s framing with a notebook instead of a rewrite rule —
/// which is exactly why the two proxies were merged before this was built rather than after.
pub struct CassetteRecorder {
    /// The port the debugger attaches to instead of the probe's own.
    pub port: u16,
    exchanges: Arc<Mutex<Vec<Exchange>>>,
    events: Arc<Mutex<usize>>,
    finished: Arc<Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>>,
    _relay: Relay,
}

impl CassetteRecorder {
    /// How long [`finish`](Self::finish) waits for the debugger to hang up before writing anyway.
    const DRAIN: Duration = Duration::from_secs(5);

    /// Listen on a fresh port, forwarding to `target_port` and recording every pair.
    ///
    /// # Errors
    /// Returns the reason as text if the port cannot be bound.
    pub fn start(target_port: u16) -> Result<Self, String> {
        let exchanges: Arc<Mutex<Vec<Exchange>>> = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(0usize));
        let finished: Arc<Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>> = Arc::new(Mutex::new(None));

        let (tape, seen, done) = (Arc::clone(&exchanges), Arc::clone(&events), Arc::clone(&finished));
        let relay = Relay::start("cassette recorder", Some(target_port), move |client, server| {
            let Some(server) = server else { return };
            let (tape, seen) = (Arc::clone(&tape), Arc::clone(&seen));
            // Nothing refused either, for the same reason nothing is rewritten below.
            let flag = wire_framed(client, server, vec![], move |from| {
                match from {
                    FromDebuggee::Reply { command, request, reply } => {
                        // Nothing is rewritten — the recording must be of the session that really happened,
                        // not of one this proxy shaped.
                        if let Ok(mut v) = tape.lock() {
                            v.push(Exchange {
                                set: command.0,
                                command: command.1,
                                request: request.to_vec(),
                                error: reply_error(reply),
                                reply: reply.get(JDWP_HEADER..).unwrap_or_default().to_vec(),
                            });
                        }
                    }
                    FromDebuggee::Event(_) => {
                        if let Ok(mut n) = seen.lock() {
                            *n += 1;
                        }
                    }
                }
                None
            });
            if let Ok(mut slot) = done.lock() {
                *slot = Some(flag);
            }
        })?;
        Ok(Self { port: relay.port, exchanges, events, finished, _relay: relay })
    }

    /// Wait for the debugger to hang up, then hand back what was recorded.
    ///
    /// The wait is the point. A recording written while the connection is still open loses whatever the
    /// debugger does on its way out — `debug.panic` and the resume behind it, in this harness — and a
    /// cassette missing the last few exchanges does not fail when it is written. It fails much later, as a
    /// replay miss in a test that looks unrelated.
    pub fn finish(self, title: &str) -> Cassette {
        let deadline = Instant::now() + Self::DRAIN;
        while Instant::now() < deadline {
            let done = self
                .finished
                .lock()
                .ok()
                .and_then(|s| s.clone())
                .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed));
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let exchanges = self.exchanges.lock().map(|v| v.clone()).unwrap_or_default();
        let events_seen = self.events.lock().map_or(0, |n| *n);
        if events_seen > 0 {
            eprintln!(
                "note: recorded {events_seen} composite event(s) that will NOT be replayed — this \
                 cassette is a faithful record of the COMMANDS in the session and nothing more"
            );
        }
        let served = vec![false; exchanges.len()];
        Cassette {
            title: title.to_string(),
            recorded_from: String::new(),
            note: String::new(),
            events_seen,
            exchanges,
            served,
        }
    }
}

/// A JDWP endpoint with no JVM behind it, answering out of a cassette.
///
/// From the debugger's side this is indistinguishable from a debuggee: it accepts a connection, returns the
/// handshake, and replies to commands. From this side it is a lookup table.
pub struct ReplayServer {
    /// The port to attach to. There is nothing else listening on it — that is the entire point.
    pub port: u16,
    misses: Arc<Mutex<Vec<String>>>,
    _relay: Relay,
}

impl ReplayServer {
    /// Serve `cassette` on a fresh port.
    ///
    /// # Errors
    /// Returns the reason as text if the port cannot be bound.
    pub fn start(cassette: &Cassette) -> Result<Self, String> {
        let misses: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (tape, log) = (cassette.clone(), Arc::clone(&misses));
        let relay = Relay::start("replay server", None, move |client, _| {
            // A fresh tape per connection, so a reconnecting debugger gets the recording from the top
            // rather than the leftovers of the last one.
            let mut tape = tape.clone();
            tape.rewind();
            let log = Arc::clone(&log);
            let Ok(mut out) = client.try_clone() else { return };
            std::thread::spawn(move || {
                read_frames(client, |frame| match frame {
                    Frame::Handshake(_) => std::io::Write::write_all(&mut out, JDWP_HANDSHAKE).is_ok(),
                    Frame::Packet(pkt) => serve(pkt, &mut tape, &mut out, &log),
                });
                let _ = out.shutdown(std::net::Shutdown::Both);
            });
        })?;
        Ok(Self { port: relay.port, misses, _relay: relay })
    }

    /// Every request this cassette could not answer, in the order they arrived.
    pub fn misses(&self) -> Vec<String> {
        self.misses.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// The misses, **acknowledged** — drained, so the [`Drop`] backstop below stays quiet.
    ///
    /// For the one kind of test that wants a miss: the one proving that a miss is loud. Everything else
    /// should be reading [`misses`](Self::misses) and failing on it.
    pub fn take_misses(&self) -> Vec<String> {
        self.misses.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    }

    /// Fail the test if anything went unanswered, naming what.
    ///
    /// Worth calling explicitly at the end of a replay test even though [`Drop`] checks too: called here
    /// the failure points at the test, and the message is not competing with whatever else is unwinding.
    pub fn assert_no_misses(&self) {
        let misses = self.misses();
        assert!(misses.is_empty(), "{}", miss_report(&misses));
    }
}

impl Drop for ReplayServer {
    /// The backstop for a replay test that forgot to check.
    ///
    /// This repo's recurring failure mode is a green run of nothing — a SIGKILL'd coverage counter, an
    /// undetectable JDK, a filter matching no tests. A cassette that quietly failed to answer would be the
    /// next one, so the miss log fails the test even if nobody asks it to. Skipped while already panicking,
    /// because a panic during unwinding aborts the process and would replace a readable assertion failure
    /// with a stack trace.
    fn drop(&mut self) {
        let misses = self.misses();
        assert!(misses.is_empty() || std::thread::panicking(), "{}", miss_report(&misses));
    }
}

/// Answer one request out of the tape, or fail loudly. `false` hangs the connection up.
fn serve(
    pkt: &[u8],
    tape: &mut Cassette,
    out: &mut std::net::TcpStream,
    log: &Arc<Mutex<Vec<String>>>,
) -> bool {
    // A reply arriving FROM the debugger answers an event we never sent, so there is nothing to look up
    // and nothing to say. Dropping it is right; treating it as a request would report a phantom miss.
    if pkt.get(8).copied().is_some_and(|f| f & JDWP_REPLY_FLAG != 0) {
        return true;
    }
    let (Some(id), Some(set), Some(cmd)) = (packet_id(pkt), pkt.get(9).copied(), pkt.get(10).copied()) else {
        return false;
    };
    let request = pkt.get(JDWP_HEADER..).unwrap_or_default();
    if let Some((error, payload)) = tape.answer(set, cmd, request) {
        return std::io::Write::write_all(out, &reply_packet(id, error, &payload)).is_ok();
    }

    // The loud path. No reply of any kind goes back: an error reply is a perfectly plausible thing for a
    // JVM to say, so a cassette that produced one on a miss would let a test pass while proving nothing
    // about the branch it thought it was in. Hanging up makes the tool call fail as a transport error,
    // which is unmistakable, and the message below says which command to add and what its payload was.
    let miss = format!(
        "{} (set {set}, command {cmd}) with request payload {}",
        command_name(set, cmd),
        if request.is_empty() { "<none>".to_string() } else { hex(request) }
    );
    eprintln!("CASSETTE MISS: no recorded reply for {miss}");
    if let Ok(mut v) = log.lock() {
        v.push(miss);
    }
    false
}

/// The assertion message for a set of misses. One shape, so both the explicit check and the `Drop` backstop
/// say the same thing.
fn miss_report(misses: &[String]) -> String {
    format!(
        "the cassette could not answer {} request(s), and a replay that cannot answer proves nothing:\n  \
         {}\n\
         Either the recording is missing them (re-record) or the debugger asked something new (add the \
         exchange to the cassette by hand — the payload above is the key).",
        misses.len(),
        misses.join("\n  ")
    )
}

/// A reply packet's JDWP error code, from bytes 9..11.
fn reply_error(pkt: &[u8]) -> u16 {
    pkt.get(9..11).and_then(|s| <[u8; 2]>::try_from(s).ok()).map_or(0, u16::from_be_bytes)
}

/// Bytes as lowercase hex — the form a payload takes in a cassette, and what anything synthesising one
/// by hand has to produce.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Hex back to bytes, tolerating the whitespace and upper case a hand edit will contain.
fn unhex(text: &str) -> Result<Vec<u8>, String> {
    let clean: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() % 2 != 0 {
        return Err(format!("{} hex digits is an odd number, so it is not whole bytes", clean.len()));
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    let digits: Vec<char> = clean.chars().collect();
    for pair in digits.chunks(2) {
        let s: String = pair.iter().collect();
        out.push(u8::from_str_radix(&s, 16).map_err(|_| format!("`{s}` is not a hex byte"))?);
    }
    Ok(out)
}

/// How many bytes of payload go on one line of a cassette. 32 keeps a line inside a terminal even with the
/// indent, and makes an offset easy to count to when the thing being edited is a length prefix.
const BYTES_PER_LINE: usize = 32;

/// A payload rendered as a JSON value: one string when it is short, an array of lines when it is not.
fn hex_field(bytes: &[u8], indent: usize) -> String {
    if bytes.len() <= BYTES_PER_LINE {
        return format!("\"{}\"", hex(bytes));
    }
    let pad = " ".repeat(indent + 2);
    let lines: Vec<String> = bytes.chunks(BYTES_PER_LINE).map(|c| format!("{pad}\"{}\"", hex(c))).collect();
    format!("[\n{}\n{}]", lines.join(",\n"), " ".repeat(indent))
}

/// Read a payload field, which may be a string or an array of strings.
fn hex_value(v: Option<&serde_json::Value>) -> Result<Vec<u8>, String> {
    match v {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::String(s)) => unhex(s),
        Some(serde_json::Value::Array(parts)) => {
            let mut out = Vec::new();
            for p in parts {
                let s = p.as_str().ok_or_else(|| "an array entry is not a string".to_string())?;
                out.extend_from_slice(&unhex(s)?);
            }
            Ok(out)
        }
        Some(other) => Err(format!("expected a hex string or an array of them, got {other}")),
    }
}

/// A string as JSON, escaped by the library rather than by hand.
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// `CommandSet.Command` for a `(set, command)` pair, or `Set9.Cmd42` for one this table does not name.
///
/// The label is not decoration. A cassette is meant to be edited, and the difference between finding the
/// `ThreadReference.Frames` reply and finding "the one with set 11" is the difference between a format that
/// invites a hand edit and one that only tolerates it.
pub fn command_name(set: u8, command: u8) -> String {
    for (s, set_name, commands) in COMMAND_SETS {
        if *s != set {
            continue;
        }
        for (c, name) in *commands {
            if *c == command {
                return format!("{set_name}.{name}");
            }
        }
        return format!("{set_name}.Cmd{command}");
    }
    format!("Set{set}.Cmd{command}")
}

/// One command inside a set: its number and its name.
type NamedCommand = (u8, &'static str);

/// One command set: its number, its name, and the commands in it.
type NamedCommandSet = (u8, &'static str, &'static [NamedCommand]);

/// The JDWP command sets, from the protocol specification. Only names — nothing here decodes a payload,
/// because a cassette stores bytes and decoding them would be a second protocol implementation to keep
/// honest.
const COMMAND_SETS: &[NamedCommandSet] = &[
    (
        1,
        "VirtualMachine",
        &[
            (1, "Version"),
            (2, "ClassesBySignature"),
            (3, "AllClasses"),
            (4, "AllThreads"),
            (5, "TopLevelThreadGroups"),
            (6, "Dispose"),
            (7, "IDSizes"),
            (8, "Suspend"),
            (9, "Resume"),
            (10, "Exit"),
            (11, "CreateString"),
            (12, "Capabilities"),
            (13, "ClassPaths"),
            (14, "DisposeObjects"),
            (15, "HoldEvents"),
            (16, "ReleaseEvents"),
            (17, "CapabilitiesNew"),
            (18, "RedefineClasses"),
            (19, "SetDefaultStratum"),
            (20, "AllClassesWithGeneric"),
            (21, "InstanceCounts"),
            (22, "AllModules"),
        ],
    ),
    (
        2,
        "ReferenceType",
        &[
            (1, "Signature"),
            (2, "ClassLoader"),
            (3, "Modifiers"),
            (4, "Fields"),
            (5, "Methods"),
            (6, "GetValues"),
            (7, "SourceFile"),
            (8, "NestedTypes"),
            (9, "Status"),
            (10, "Interfaces"),
            (11, "ClassObject"),
            (12, "SourceDebugExtension"),
            (13, "SignatureWithGeneric"),
            (14, "FieldsWithGeneric"),
            (15, "MethodsWithGeneric"),
            (16, "Instances"),
            (17, "ClassFileVersion"),
            (18, "ConstantPool"),
            (19, "Module"),
        ],
    ),
    (3, "ClassType", &[(1, "Superclass"), (2, "SetValues"), (3, "InvokeMethod"), (4, "NewInstance")]),
    (4, "ArrayType", &[(1, "NewInstance")]),
    (5, "InterfaceType", &[(1, "InvokeMethod")]),
    (
        6,
        "Method",
        &[
            (1, "LineTable"),
            (2, "VariableTable"),
            (3, "Bytecodes"),
            (4, "IsObsolete"),
            (5, "VariableTableWithGeneric"),
        ],
    ),
    (8, "Field", &[]),
    (
        9,
        "ObjectReference",
        &[
            (1, "ReferenceType"),
            (2, "GetValues"),
            (3, "SetValues"),
            (5, "MonitorInfo"),
            (6, "InvokeMethod"),
            (7, "DisableCollection"),
            (8, "EnableCollection"),
            (9, "IsCollected"),
            (10, "ReferringObjects"),
        ],
    ),
    (10, "StringReference", &[(1, "Value")]),
    (
        11,
        "ThreadReference",
        &[
            (1, "Name"),
            (2, "Suspend"),
            (3, "Resume"),
            (4, "Status"),
            (5, "ThreadGroup"),
            (6, "Frames"),
            (7, "FrameCount"),
            (8, "OwnedMonitors"),
            (9, "CurrentContendedMonitor"),
            (10, "Stop"),
            (11, "Interrupt"),
            (12, "SuspendCount"),
            (13, "OwnedMonitorsStackDepthInfo"),
            (14, "ForceEarlyReturn"),
        ],
    ),
    (12, "ThreadGroupReference", &[(1, "Name"), (2, "Parent"), (3, "Children")]),
    (13, "ArrayReference", &[(1, "Length"), (2, "GetValues"), (3, "SetValues")]),
    (14, "ClassLoaderReference", &[(1, "VisibleClasses")]),
    (15, "EventRequest", &[(1, "Set"), (2, "Clear"), (3, "ClearAllBreakpoints")]),
    (16, "StackFrame", &[(1, "GetValues"), (2, "SetValues"), (3, "ThisObject"), (4, "PopFrames")]),
    (17, "ClassObjectReference", &[(1, "ReflectedType")]),
    (18, "ModuleReference", &[(1, "Name"), (2, "ClassLoader")]),
    (64, "Event", &[(100, "Composite")]),
];

/// Where the checked-in cassettes live. Named by the test that plays them.
pub fn cassette_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes").join(format!("{name}.json"))
}

/// Set when a recording run should overwrite the checked-in fixtures rather than only round-trip through a
/// temporary file. Re-recording is a deliberate act — it needs a JDK and it changes a reviewed artefact —
/// so it does not happen just because someone ran the suite.
pub const RERECORD_ENV: &str = "JDWP_RERECORD_CASSETTES";

/// Whether this run was asked to overwrite the checked-in cassettes.
pub fn rerecording() -> bool {
    std::env::var(RERECORD_ENV).is_ok_and(|v| v != "0" && !v.is_empty())
}
