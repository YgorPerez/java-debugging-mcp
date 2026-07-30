// Typed tool arguments — the single source of truth for each tool's parameters.
//
// Each struct derives both `Deserialize` (how handlers read the arguments) and `JsonSchema`
// (how tools.rs advertises the schema to the client). Because both come from ONE definition,
// the advertised schema can't drift from what the handler actually parses — the class of bug
// that left `max_variable_depth`/`timeout_ms` dead and `max_result_length` reporting 500 while
// the code used 4000. Field doc-comments become the schema `description`.

use schemars::JsonSchema;
use serde::Deserialize;

fn default_host() -> String {
    "localhost".to_string()
}
const fn default_port() -> u16 {
    5005
}
const fn default_max_frames() -> usize {
    20
}
const fn default_true() -> bool {
    true
}
const fn default_max_result_length() -> usize {
    2000
}
const fn default_limit() -> usize {
    40
}
// Higher than `default_limit`: a class listing is one short line each, and an app server loads
// thousands, so the useful default shows enough of a package to recognise it without a second call.
const fn default_class_limit() -> usize {
    100
}
// A ~41-line window: enough to hold the method a stack frame points into, which is the unit a caller
// chasing that frame is actually reading, without pulling the file's neighbours in with it.
const fn default_source_context() -> usize {
    20
}
// The ceiling on source lines in one reply, whichever way they were chosen. Deliberately far above
// `default_source_context` so it only ever bites on `whole_file`, where the file's own size is the
// only other bound — and a 2000-line class dumped into context is the cost being capped.
const fn default_source_max_lines() -> usize {
    400
}
const fn default_trace_limit() -> usize {
    50
}
const fn default_event_limit() -> usize {
    1
}
const fn default_max_depth() -> usize {
    2
}
const fn default_max_children() -> usize {
    16
}
const fn default_trace_frames() -> usize {
    crate::handlers::DEFAULT_TRACE_FRAMES
}
// `trace_max_length` unset. Deliberately NOT a number: the two capture-time caps it raises are
// different from each other (100 for a local, 200 for the `trace_expr` result), so there is no single
// scalar that could stand for "leave them alone", and inventing one would change the default output of
// whichever of the two it did not match. `None` means exactly what it says — the caller expressed no
// preference — and `trace_lengths` in `handlers` is the one place that turns it into the pair.
//
// Written as a shared fn beside `default_trace_frames` for the reason that one is: four arming tools
// read this argument, and a per-struct `#[serde(default)]` is four places for the meaning of "unset" to
// drift apart in.
const fn default_trace_max_length() -> Option<usize> {
    None
}
const fn default_dump_frames() -> usize {
    8
}
const fn default_max_suspend_ms() -> u64 {
    crate::handlers::DEFAULT_MAX_SUSPEND_MS
}

/// Parse an optional hex thread id like "0x2" (or "2") into a raw id.
pub fn parse_thread_id(s: Option<&str>) -> Option<u64> {
    s.and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

/// Deserialize tool arguments into a typed struct, tolerating a missing/`null` arguments value
/// (treated as an empty object so all-optional structs still get their defaults).
pub fn parse<T: serde::de::DeserializeOwned>(args: &serde_json::Value) -> Result<T, String> {
    let v = if args.is_null() { serde_json::Value::Object(serde_json::Map::new()) } else { args.clone() };
    serde_json::from_value(v).map_err(|e| format!("Invalid arguments: {e}"))
}

/// Arguments for `debug.launch` (LAUNCH-1).
///
/// **What this tool is for, and what it is not.** Every other tool here assumes the JVM belongs to someone
/// else — that is where the watchdog, `debug.panic`, trace mode, `JDWP_READONLY` and SAFE-1's
/// resume-on-disconnect all come from. This one starts a JVM that is *yours*: nobody else's requests are on
/// it, suspending it costs nobody anything, and `suspend=y` becomes reachable, which is the only way to
/// break on code that runs during initialisation. It is not a way to manage an app server — a long-running
/// deployment should be started by whatever normally starts it, and attached to.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaunchArgs {
    /// Main class to run, e.g. `com.example.Main`. Give this **or** `jar`, not both. Needs `classpath`
    /// unless the class is on the default one.
    #[serde(default)]
    pub main_class: Option<String>,
    /// Executable jar to run (`java -jar <jar>`). Give this **or** `main_class`.
    #[serde(default)]
    pub jar: Option<String>,
    /// Classpath entries, e.g. `["target/classes", "libs/dep.jar"]` — joined with the platform separator.
    /// Relative paths resolve against `working_dir`.
    #[serde(default)]
    pub classpath: Option<Vec<String>>,
    /// Extra JVM arguments, e.g. `["-Xmx512m", "-Dspring.profiles.active=dev"]`. Passed through verbatim,
    /// before the main class. The `-agentlib:jdwp=…` argument is added for you and must not be given here.
    #[serde(default)]
    pub jvm_args: Option<Vec<String>>,
    /// Arguments for the program's own `main(String[])`.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Working directory for the JVM. Defaults to this server's own.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Suspend the JVM at startup, before a single line of the program runs — **default true**, and the
    /// main reason this tool exists.
    ///
    /// `suspend=y` is unreachable when you attach to a JVM someone else started: by the time you can
    /// connect, static initialisers, framework bootstrapping and anything else that runs once have already
    /// run. With this you arm the stop points first and then `debug.continue`. Pass false for a JVM you
    /// only want running and attached.
    #[serde(default = "default_true")]
    pub suspend: bool,
    /// JDWP port. `0` (the default) picks a free one, which is usually right — the port only matters if
    /// something else needs to reach it too.
    #[serde(default)]
    pub port: u16,
    /// Which JDK to run: a home directory whose `bin/java` is used. Defaults to `JAVA_HOME`, then to `java`
    /// on `PATH`. The reply always names the binary it actually ran, because "which JDK" is exactly the
    /// question a version-dependent bug turns on.
    #[serde(default)]
    pub java_home: Option<String>,
    /// Leave the JVM RUNNING when the session disconnects, instead of terminating it.
    ///
    /// The default is to terminate: this server started the process, so it owns it, and a JVM left behind
    /// with an open JDWP port that no session knows about is worse than one that stops when you are done
    /// with it. Set this when the point of the run is the program rather than the debugging — but then its
    /// lifetime is yours, and nothing here will clean it up.
    #[serde(default)]
    pub detach_on_disconnect: bool,
    /// Open the session read-only, exactly as `debug.attach` does. Rarely what you want on a JVM you
    /// started yourself, but `JDWP_READONLY` still applies deployment-wide and cannot be relaxed here
    /// (SAFE-3).
    #[serde(default)]
    pub read_only: bool,
    /// Directories `debug.source` searches for `.java` files — see `debug.attach`.
    #[serde(default)]
    pub source_roots: Option<Vec<String>>,
    /// Directories holding freshly compiled classes for `debug.reload_class` / `debug.check_stale` — see
    /// `debug.attach`.
    #[serde(default)]
    pub class_roots: Option<Vec<String>>,
}

/// A class argument that takes **one** pattern or **several** (FILT-4).
///
/// Deserialises from either a JSON string or an array of strings, so `"com.example.Order"` and
/// `["com.example.Order", "com.example.*Repo"]` are both valid and nothing written before FILT-4 has to
/// change. The reply shape follows the same rule in reverse: a call that can only produce ONE stop point
/// still gets exactly the single-stop-point reply it always got, and only a call that can produce several
/// gets the per-target breakdown. That is deliberate — a batch must not make the ordinary arming harder
/// to read, and partial success (2 armed, 1 deferred, 1 refused) is the *normal* outcome of a batch, so
/// it needs a shape of its own rather than an error that discards the two that worked.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ClassPatterns {
    /// One pattern — how every caller before FILT-4 spelled it.
    One(String),
    /// Several patterns, resolved and armed independently of one another.
    Many(Vec<String>),
}

impl ClassPatterns {
    /// The patterns: trimmed, blanks dropped, duplicates removed, original order kept.
    ///
    /// Deduped because the same pattern twice would arm two stop points on one location — never what a
    /// caller means, and otherwise only discoverable by spending the JDWP requests to do it.
    pub fn list(&self) -> Vec<String> {
        let raw: &[String] = match self {
            Self::One(s) => std::slice::from_ref(s),
            Self::Many(v) => v.as_slice(),
        };
        let mut out: Vec<String> = Vec::with_capacity(raw.len());
        for p in raw {
            let t = p.trim();
            if !t.is_empty() && !out.iter().any(|q| q == t) {
                out.push(t.to_string());
            }
        }
        out
    }
}

/// Default ceiling on how many loaded classes ONE wildcard pattern may arm (FILT-3).
///
/// A wildcard is N arming operations whose N the caller cannot see before making the call — `com.*` on a
/// real app server is thousands of line-table lookups and thousands of live stop points. So the expansion
/// is bounded by default and says what it left out, rather than being fast to type and slow to regret.
/// Raise it with `max_classes` when you mean it; it is clamped at [`MAX_CLASSES_CEILING`].
pub const DEFAULT_MAX_CLASSES: usize = 20;

/// Hard ceiling on `max_classes`, however large a number is passed.
///
/// Not a taste judgement: every armed location is a live JDWP event request on a JVM that is usually
/// shared, and a caller who asks for 5000 has almost certainly mistyped a pattern rather than decided
/// something. The reply says when it clamped.
pub const MAX_CLASSES_CEILING: usize = 200;

const fn default_max_classes() -> usize {
    DEFAULT_MAX_CLASSES
}

/// Arguments for debug.attach.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachArgs {
    /// JVM host (e.g. "localhost").
    #[serde(default = "default_host")]
    pub host: String,
    /// JDWP port (e.g. 5005 or 8787).
    #[serde(default = "default_port")]
    pub port: u16,
    /// Open the session read-only: method invocation, `set_value` and `force_return` are refused, so
    /// pointing at a production JVM can't accidentally mutate it. Reads (locals, fields, statics,
    /// arrays, `get_stack`, watchpoint/exception reporting) still work; collection expansion and
    /// `toString()` rendering fall back to shallow, because they invoke methods in the debuggee. A
    /// guard against accident, NOT a security boundary. Also forced on by the `JDWP_READONLY` env var.
    #[serde(default)]
    pub read_only: bool,
    /// Directories `debug.source` searches for a class's `.java` file, e.g.
    /// `["/srv/app/src/main/java"]`. A root is where the PACKAGE TREE starts, not the project root:
    /// the file is looked for at `<root>/<package as directories>/<file the JVM reports>`. Plain
    /// directories only — sources inside JARs are not read. Given here they replace the
    /// `JDWP_SOURCE_ROOTS` environment default for this session; omitted, that default applies.
    #[serde(default)]
    pub source_roots: Option<Vec<String>>,
    /// Directories holding freshly COMPILED classes, e.g. `["/srv/app/target/classes"]` — where
    /// `debug.reload_class` reads the bytes it ships to the JVM. Same shape as `source_roots` and a
    /// different tree: a class root is where the package tree starts in the *build output*
    /// (`target/classes`), not in the sources (`src/main/java`). The file is looked for at
    /// `<root>/<package as directories>/<SimpleName>.class`. Given here they replace the
    /// `JDWP_CLASS_ROOTS` environment default for this session; omitted, that default applies.
    #[serde(default)]
    pub class_roots: Option<Vec<String>>,
}

/// Arguments for `debug.set_line_stop`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetBreakpointArgs {
    /// Where to break: an EXACT class name (`com.example.MyClass`), a WILDCARD matching many loaded
    /// classes (`com.example.*`, `*.OrderService`, `*Order*` — FILT-3), or a LIST of either
    /// (`["com.example.Order", "com.example.*Repo"]` — FILT-4).
    ///
    /// The three behave differently, and the difference is worth knowing before you type one:
    ///
    /// - **Exact** arms one breakpoint. If the class is not loaded yet the breakpoint is *deferred* on a
    ///   `CLASS_PREPARE` watch and arms itself when the class loads.
    /// - **Wildcard** needs `method` and refuses `line` — a line number means a different thing in every
    ///   class, so it is not portable across a pattern. It arms ONE STOP POINT PER MATCHING LOADED CLASS,
    ///   each with its own `bp_…` id, and additionally keeps watching: a class matching the pattern that
    ///   loads later (a generated proxy, a lazily-initialised implementation) is armed too. The whole
    ///   family is addressable as one `bpset_…` id, so `debug.clear_stop_point`/`debug.toggle_stop_point`
    ///   can drop or silence it — including the watch for future classes — in one call. Bounded by
    ///   `max_classes`, and the reply states how many classes it armed, since that count is the one thing
    ///   a wildcard hides from the caller.
    /// - **List** resolves each entry independently by the two rules above. Nothing is aborted by one
    ///   entry failing: the reply reports every pattern's outcome, because 2-armed-1-deferred-1-refused is
    ///   the normal result of a batch.
    ///
    /// A JNI signature (`Lcom/example/MyClass;`) is accepted for an exact class, and dots or slashes work
    /// as the separator either way.
    pub class_pattern: ClassPatterns,
    /// Line number (optional if `method` is given). Refused with a wildcard `class_pattern` — see there.
    #[serde(default)]
    pub line: Option<i32>,
    /// Method name (optional). If given without `line`, breaks at the method's first line.
    #[serde(default)]
    pub method: Option<String>,
    /// Only stop on the Nth hit (optional).
    ///
    /// **It counts hits, not matches, so it does not compose with `condition` the way it reads.** This is
    /// JDWP's `Count` modifier and it expires inside the JVM: `hit_count: 5` with a condition means
    /// "evaluate the condition on the 5th hit", not "the 5th hit where the condition holds" — and if the
    /// condition is false on that hit, the stop point is spent and will never fire again. Use one or the
    /// other unless you mean exactly that.
    #[serde(default)]
    pub hit_count: Option<i32>,
    /// Only stop when this thread (hex id) hits it (optional).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Only stop when this boolean expression is true, evaluated in the hit frame. Supports
    /// `expr OP expr` (==, !=, <, >, <=, >=) and boolean method chains, e.g.
    /// `reserva.getReservaPacote().getReservaHotelList().size() > 0`, `getSgMoeda() == "BRL"`, or
    /// `total > Config.LIMITE`. Either side may be a literal or an expression; two Strings compare
    /// by content, other objects by identity.
    ///
    /// **A non-matching hit holds only the thread that hit it — the rest of the JVM keeps running.** That
    /// is what makes this argument safe to arm on a hot line on a shared instance, and it is why it is
    /// worth preferring over a bare breakpoint there.
    ///
    /// **The trade, stated because no reply can show it to you:** when the condition DOES hold, the
    /// debugger suspends the rest of the VM at that point, and the other threads keep running for the one
    /// round trip that takes. So the state you then read is the state a moment *after* the hit, not the
    /// state at the instant of it — the hit thread's own frame is exactly as the condition saw it, but
    /// anything it points at may have been touched by another thread in between. A stop point with no
    /// `condition` does not have this gap, because the JVM freezes everything before it tells us anything.
    /// If the escalation fails, `debug.get_last_event` says so explicitly and states whether the
    /// application is still running, rather than reporting either half alone.
    #[serde(default)]
    pub condition: Option<String>,
    /// Logpoint mode: on hit, snapshot (location, thread, in-scope locals/args, optional
    /// `trace_expr`) into a ring buffer and resume immediately WITHOUT suspending — safe on a
    /// shared instance where a normal breakpoint could freeze a request thread. Read snapshots
    /// with `debug.get_traces`.
    #[serde(default)]
    pub trace: bool,
    /// Only with `trace:true` — an expression to evaluate in the hit frame and record alongside
    /// the snapshot (e.g. `reserva.getStatus()`).
    #[serde(default)]
    pub trace_expr: Option<String>,
    /// Only with `trace:true` — disarm this logpoint automatically after recording this many hits, so
    /// a hot line can't flood the debuggee (defaults to 200). Pass 0 for no limit.
    ///
    /// **This bound is load-bearing, not a formality.** Capture is serialised through the single JDWP
    /// connection and one event pump, so a traced stop point tops out at roughly **720 hits/s** with the
    /// default 3 caller frames, or **~1160/s** with `trace_frames: 0`. Past that the debugger — not the
    /// application — is the bottleneck, and every further hit queues behind the ones being captured. At
    /// the default 200 the exposure is a sub-second blip, which is most of the reason trace mode is safe
    /// to leave armed at all. **`0` on a site that fires thousands of times a second turns that blip into
    /// sustained throttling**: it is the single setting that removes the protection, so choose it
    /// knowingly. Loopback measurement against a trivial `WildFly` endpoint (#22) — the absolute figures
    /// move with hardware, the existence of a hits/s ceiling does not. **You do not have to trust those
    /// figures**: once the stop point has fired, `debug.list_stop_points` reports the mean capture measured
    /// on the JVM you are actually attached to — invert it for this ceiling — plus the rate hits are
    /// arriving at and the share of the window spent capturing (TRACE-7).
    #[serde(default)]
    pub trace_max_hits: Option<u32>,
    /// Only with `trace:true` — how many CALLER frames to record above the hit, so a snapshot says
    /// **which path reached it**, not just that it fired (default 3; 0 for the hit frame alone, capped
    /// at 20). Callers are recorded as `class.method:line` locations only — no locals, no invocation —
    /// so this stays safe in a read-only session. Each frame costs JVM round trips on *every* hit, so
    /// keep it small on a hot line and pair it with `trace_max_hits`.
    ///
    /// The measured price of that depth: capture costs ~0.86ms per hit before any callers, and the
    /// default 3 frames add ~0.53ms on top (**+62%**), lowering the ceiling from ~1160 to ~720 hits/s.
    /// Kept at 3 regardless, because the chain is usually the answer rather than context — but
    /// `trace_frames: 0` is the cheap mode when the site is hot and you only need that it fired.
    /// Loopback measurement against a trivial `WildFly` endpoint (#22); `debug.list_stop_points` reports
    /// what the depth you chose is costing on *your* JVM once hits have landed.
    #[serde(default = "default_trace_frames")]
    pub trace_frames: usize,
    /// Only with `trace:true` — raise the per-value length cap on each capture, for a payload that does
    /// not fit the frugal defaults: a JSON body, a SOAP envelope, a `+=`-built SQL string. Unset keeps
    /// them exactly as they were — **100** characters for each in-scope local, **200** for the
    /// `trace_expr` result, the method-exit `returned` value and a watchpoint's old → new pair. Ceiling
    /// 4000; a larger request is clamped and the reply says so, and `0` is read as the ceiling rather
    /// than as "no limit" (which is what it means on `trace_max_hits`, and cannot mean here).
    ///
    /// **Truncation happens at CAPTURE time**, which is why this is an arming argument rather than a
    /// `debug.get_traces` one: the cut string is what the snapshot stores, so the remainder is gone and
    /// no later read can recover it. On a shared instance the workaround — suspend, then `debug.evaluate`
    /// with a large `max_result_length` — is precisely the thing trace mode exists to avoid.
    ///
    /// **The two defaults differ deliberately, and ONE argument raises both.** The locals are context,
    /// captured whether you asked for them or not, while `trace_expr` is the payload you named, so it
    /// already gets twice the room; both are frugal because a trace may fire hundreds of times into a
    /// bounded buffer. One knob rather than two because a caller raising the cap wants the payload and
    /// should not have to work out which of two numbers governs the value in front of them.
    ///
    /// It is still a bound. Buffer memory is roughly this cap × the hits recorded, and `trace_max_hits`
    /// defaults to 200 — so 4000 is ~800KB per captured value on one stop point at its default budget,
    /// which is where the ceiling comes from.
    #[serde(default = "default_trace_max_length")]
    pub trace_max_length: Option<usize>,
    /// Most loaded classes ONE wildcard pattern may arm (default 20, clamped at 200 — FILT-3).
    ///
    /// Only wildcards spend this; an exact class name arms one breakpoint and ignores it. When a pattern
    /// matches more classes than this, the reply says how many it left out and how to see them
    /// (`debug.list_classes {filter}` with the same pattern), rather than truncating quietly. The cap also
    /// applies to classes that load *later*: the family stops arming new ones once it is full, and
    /// `debug.list_stop_points` reports that it is.
    #[serde(default = "default_max_classes")]
    pub max_classes: usize,
    // NOTE: `session_id` is a cross-cutting argument handled uniformly by `resolve_session`
    // (from the raw arguments) for every tool, so it is intentionally not a typed field here.
}

/// Arguments for `debug.toggle_stop_point` (BP-1).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToggleBreakpointArgs {
    /// Stop-point ID (`bp_…`) from `debug.list_stop_points`.
    pub breakpoint_id: String,
    /// Desired state: `false` clears the JDWP request but keeps the definition (`condition`,
    /// `trace_expr`) so it can be re-armed later; `true` re-arms it. Omit to flip the current state.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Arguments for `debug.get_traces`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTracesArgs {
    /// Max snapshots to return, most recent last (default 50).
    #[serde(default = "default_trace_limit")]
    pub limit: usize,
    /// Clear the trace buffer after returning it.
    #[serde(default)]
    pub clear: bool,
    /// Only snapshots from this stop point id (`bp_…` / `exc_…` / `watch_…`) — read "the throws from
    /// `exc_4`" without eyeballing everything (TRACE-4).
    #[serde(default)]
    pub bp_id: Option<String>,
    /// Only snapshots whose class contains this substring (case-insensitive).
    #[serde(default)]
    pub class_filter: Option<String>,
    /// Only snapshots newer than this sequence number, so a poller can ask for just what's new since
    /// its last read (the `#seq` shown on each record).
    #[serde(default)]
    pub since: Option<u64>,
}

/// Arguments for `debug.get_last_event`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetLastEventArgs {
    /// How many buffered events to return, most recent LAST (default 1 — just the latest). Raise it
    /// to catch up when the reply says events are pending, e.g. after a broad exception breakpoint.
    #[serde(default = "default_event_limit")]
    pub limit: usize,
    /// Discard the events returned, so the next call only sees newer ones. Off by default, so
    /// repeated calls keep reporting the same latest hit.
    #[serde(default)]
    pub drain: bool,
}

/// Arguments for `debug.clear_stop_point`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClearBreakpointArgs {
    /// Stop-point ID from `debug.list_stop_points`.
    pub breakpoint_id: String,
}

/// Arguments for `debug.step_over` / `step_into` / `step_out`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StepArgs {
    /// Thread ID to step (optional; defaults to the last thread that hit a breakpoint).
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Arguments for `debug.get_stack`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetStackArgs {
    /// Thread ID (optional; defaults to the last thread that hit a breakpoint/step).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Maximum number of frames to return.
    #[serde(default = "default_max_frames")]
    pub max_frames: usize,
    /// Include local variables under each frame (set false for just the call chain).
    #[serde(default = "default_true")]
    pub include_variables: bool,
    /// Only show frames whose class name contains this substring (case-insensitive), e.g. your
    /// app package 'br.com.infotravel'; framework frames collapse into "… N frame(s) hidden".
    /// Big token saver on deep JVM stacks.
    #[serde(default)]
    pub package_filter: Option<String>,
    /// Expand each local recursively rather than showing objects as `Type (id=…)`. Costs many JVM
    /// round trips per frame, so pair it with `max_frames`/`package_filter` to keep the stack narrow.
    #[serde(default)]
    pub expand_objects: bool,
    /// Only with `expand_objects` — levels to expand per local (default 2).
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    /// Only with `expand_objects` — max fields/elements per node (default 16).
    #[serde(default = "default_max_children")]
    pub max_children: usize,
}

/// Arguments for debug.evaluate.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvaluateArgs {
    /// Java expression. Heads: a local, `this`, or a class (`ConfigDefaultUtils`, or fully
    /// qualified). Then chain `.field` and `.method(args)` freely, including static members
    /// (`ConfigDefaultUtils.getUrl()`). Arguments may be literals (int, `123L`, true/false, null,
    /// `"string"`) or expressions passed by reference — a local, `this.field`, or a nested call
    /// (`svc.matches(reserva)`, `foo.handle(this, cfg.getId())`).
    ///
    /// Subscripts work on arrays, `List` and `Map`:
    /// `lines[0]` (index — keeps chaining, so `lines[0].sku` works), `counts["key"]` (map lookup),
    /// `lines[2..5]` (half-open slice) and `lines[?qty > 3]` (filter). In a filter the left side is
    /// resolved against **each element** — `lines[?status == "OPEN"]`, `lines[?getQty() == 2]` — while
    /// the right side may be a literal or an expression read from the frame (`lines[?qty > limit]`).
    /// Filtering a `Map` tests its **values** and renders survivors as `key → value`, so you keep the
    /// keys you were looking for; a `Map` can't be sliced (no positional order).
    /// A slice or filter selects several values, so nothing can be chained after it.
    pub expression: String,
    /// Thread ID (optional; defaults to the last thread that hit a breakpoint).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Stack frame index (0 = current frame).
    #[serde(default)]
    pub frame_index: usize,
    /// Maximum length of the rendered result string (raise for long toString()s).
    #[serde(default = "default_max_result_length")]
    pub max_result_length: usize,
    /// Expand the result recursively instead of showing one line: walks instance fields, array
    /// elements, and the contents of `List`/`Set`/`Map`/`Optional`, with cycle detection. Needs a
    /// suspended thread for collections (it invokes `toArray`/`entrySet` in the debuggee).
    #[serde(default)]
    pub expand_objects: bool,
    /// Only with `expand_objects` — how many levels to expand (default 2). Deeper costs more JVM
    /// round trips; a total node budget caps the work regardless.
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    /// Only with `expand_objects` — max fields per object / elements per collection before
    /// "… +N more" (default 16).
    #[serde(default = "default_max_children")]
    pub max_children: usize,
}

/// Arguments for `debug.evaluate_chain` (EVAL-6).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvaluateChainArgs {
    /// The chained Java expression to walk, in exactly the form `debug.evaluate` accepts —
    /// `wsReservaCircuito.getCircuitoParametro().getConfigUhList()[0].getSqQuarto()`.
    ///
    /// A single-link expression is accepted but answers nothing this tool is for: with one link there is
    /// no "which one" to find.
    pub expression: String,
    /// Thread ID (optional; defaults to the last thread that hit a breakpoint).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Stack frame index (0 = current frame).
    #[serde(default)]
    pub frame_index: usize,
    /// Maximum length of each link's rendered value.
    #[serde(default = "default_max_result_length")]
    pub max_result_length: usize,
}

/// Arguments for `debug.list_threads`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListThreadsArgs {
    /// Only threads whose name contains this substring (case-insensitive), e.g. 'Avail' or 'task'.
    #[serde(default)]
    pub name_filter: Option<String>,
    /// Only threads currently suspended (also appends each thread's run status).
    #[serde(default)]
    pub only_suspended: bool,
    /// Max threads to return; the rest are reported as a hidden count (default 40).
    ///
    /// **Which 40 you get is a rule, and the reply states it** — the same rule `debug.thread_dump` uses,
    /// in the same words (ADR-0013; DUMP-5, #51). One thread from each distinct name with its digits
    /// ignored (`task-7` and `task-91` are one family) before a second from any, printed in creation
    /// order, with the biggest withheld groups named in the footer.
    ///
    /// It is **not** the first 40 the JVM listed: `AllThreads` order is *creation* order, and an app
    /// server creates its request pool last, so on a loaded `WildFly` the first 40 were measured to
    /// contain no application threads at all. Choosing costs one packet per thread NAME — the reply
    /// reports the figure — which is why raising `limit` is cheap here and still not the answer.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Arguments for `debug.list_classes` (DISC-1).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListClassesArgs {
    /// Narrow by class name. Matched against the dotted FQN (com.example.Order), never the JNI
    /// signature the JVM reports. Three shapes: prefix 'com.example.*', suffix `*.OrderService`,
    /// or a bare substring. Case-sensitive, because Java names are.
    #[serde(default)]
    pub filter: Option<String>,
    /// Max classes to return; the rest are reported as a hidden count.
    #[serde(default = "default_class_limit")]
    pub limit: usize,
    /// Include array types (java.lang.String[]). Off by default — they are noise when the question
    /// is which class to arm a stop point on.
    #[serde(default)]
    pub include_arrays: bool,
}

/// Arguments for `debug.list_methods` (DISC-2).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListMethodsArgs {
    /// Fully-qualified class name, e.g. com.example.OrderService. Must already be loaded — find it
    /// with `debug.list_classes` if unsure.
    pub class_name: String,
    /// Only methods whose name contains this substring (case-insensitive).
    #[serde(default)]
    pub name_filter: Option<String>,
    /// Also walk the superclass chain. Off by default: the class's own methods are what was asked
    /// for, and Object's contribute noise to every listing.
    #[serde(default)]
    pub inherited: bool,
    /// Max methods to return; the rest are reported as a hidden count.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Arguments for `debug.list_fields` (DISC-5).
///
/// Deliberately the same shape as [`ListMethodsArgs`], down to the argument names and the default
/// limit: the two answer the two halves of one question ("what can I call on this type", "what state
/// does it hold"), and a caller who has learnt one should not have to learn the other. See ADR-0015 for
/// why this is a second tool rather than a `fields:true` flag on the first.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFieldsArgs {
    /// Fully-qualified class name, e.g. com.example.OrderService. Must already be loaded — find it
    /// with `debug.list_classes` if unsure.
    pub class_name: String,
    /// Only fields whose name contains this substring (case-insensitive).
    #[serde(default)]
    pub name_filter: Option<String>,
    /// Also walk the superclass chain. Off by default: what this type itself declares is the smaller,
    /// clearer answer, and Object's contribute noise to every listing.
    #[serde(default)]
    pub inherited: bool,
    /// Max fields to return; the rest are reported as a hidden count.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Arguments for `debug.source` (DISC-3).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceArgs {
    /// Fully-qualified class name, e.g. com.example.OrderService, or an inner class
    /// com.example.Order$Line. Must already be loaded — find it with `debug.list_classes`.
    pub class_name: String,
    /// Centre the reply on this 1-based source line — the one a stack frame, a stop point or a trace
    /// snapshot reported. This is the intended way to use the tool: without it (and without
    /// `whole_file`) the reply is the JVM's answer only, and no file is read.
    #[serde(default)]
    pub line: Option<i32>,
    /// Lines of context either side of `line`.
    #[serde(default = "default_source_context")]
    pub context: usize,
    /// Return the whole file rather than a window, still capped by `max_lines`. Off by default, and
    /// it overrides `line` when both are given.
    #[serde(default)]
    pub whole_file: bool,
    /// Hard cap on how many source lines one reply may contain. Hitting it truncates loudly — the
    /// reply always says which lines of how many it is showing.
    #[serde(default = "default_source_max_lines")]
    pub max_lines: usize,
    /// Directories to search for this call only, replacing the session's roots (set at
    /// `debug.attach` or by `JDWP_SOURCE_ROOTS`). Pass `[]` to skip reading any file and get only
    /// what the JVM knows.
    #[serde(default)]
    pub source_roots: Option<Vec<String>>,
}

/// Arguments for `debug.thread_dump` (DUMP-1).
// The bools are independent MCP arguments a caller passes by name, not a parameter bag that wants
// splitting up: suspend, only_suspended and monitors_only each answer a different question about how
// much of the VM to touch, and grouping them would only make callers assemble a nested object.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadDumpArgs {
    /// Only threads whose name contains this substring (case-insensitive), e.g. 'default task' for
    /// `WildFly`'s request pool. The cheapest way to cut the cost of a dump on a JVM with hundreds of
    /// threads.
    #[serde(default)]
    pub name_filter: Option<String>,
    /// Only threads that are already suspended. On a running VM those are the only ones whose stacks
    /// can be read at all, so this is the way to get a dump with no unreadable entries in it.
    #[serde(default)]
    pub only_suspended: bool,
    /// Max threads to include; the rest are reported as a hidden count (default 40).
    ///
    /// **Which 40 you get is a rule, and the reply states it.** They are chosen by **name family** — one
    /// thread from each distinct thread name with its digits ignored (`default task-7` and
    /// `default task-91` are one family, `default I/O-3` is another) before a second from any family, so
    /// no single pool can spend every slot. The rows are then printed in creation order, and the
    /// truncation footer names the biggest groups it withheld.
    ///
    /// It is **not** the first 40 the JVM listed. JDWP `AllThreads` order is *creation* order and an app
    /// server creates its request pool last, so on a loaded `WildFly` the first 40 were measured to be
    /// entirely JVM internals, the service container and Undertow selectors, with **no application
    /// threads at all**, while 13 request workers sat 328 frames deep (TEST-8, #24; fixed in DUMP-3, #43,
    /// see ADR-0013). Raising `limit` was never the answer to that — it buys more selectors before it
    /// reaches the pool.
    ///
    /// `name_filter` (e.g. `'default task'`) is still the cheapest way to ask about one pool, and it
    /// composes: with a single family left, the round-robin *is* creation order and the dump says nothing
    /// about it.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Max frames per thread (default 8 — deliberately narrower than `debug.get_stack`, since this
    /// multiplies by the thread count). The deepest frames are the ones dropped.
    ///
    /// Kept at 8 after measuring a real `WildFly` (TEST-8, #24), where 180 of 264 threads exceeded it and a
    /// working request thread showed 8 of ~328 frames. Frame #0 is the innermost, so those 8 are *where the
    /// thread actually is*, which is the useful end — and raising the default multiplies by the thread
    /// count. For a deep read, narrow with `name_filter` and raise this, rather than widening the default.
    #[serde(default = "default_dump_frames")]
    pub max_frames: usize,
    /// Only show frames whose class name contains this substring (case-insensitive), e.g. your app
    /// package; framework frames collapse into "… N frame(s) hidden" and cost no lookups. On a
    /// `WildFly` stack this is the difference between 8 useful frames and 8 servlet-filter frames.
    #[serde(default)]
    pub package_filter: Option<String>,
    /// Include each thread's held monitors and the monitor it is blocked on (default true) — the
    /// "who holds the lock this thread is waiting for" half of a deadlock investigation. Skipped
    /// automatically, with a note, on a JVM that lacks the capability.
    #[serde(default = "default_true")]
    pub monitors: bool,
    /// Read **only** the lock state — the monitors each thread holds and the one it is blocked
    /// entering — and skip the frames entirely (default false). The cheap way to ask "who is blocked on
    /// what".
    ///
    /// Measured against a 60-thread probe: **245 packets and 33ms of suspension, against 770 and 117ms**
    /// for the same dump with stacks. The lock state costs a flat ~4 JDWP packets per thread, while each
    /// frame read adds ~3 more (method and line; class names are cached across the dump).
    ///
    /// That saving was predicted to widen against `WildFly`-depth stacks. **It does not** (TEST-8, #24). On
    /// a real `WildFly` loaded to 267 threads it cut the full dump from 467ms to 198ms and the default from
    /// 144ms to 35ms — 1.6–2.4x, *narrower* than the 3x on probes. On the **same VM idle it was slower**:
    /// 114ms against the full dump's 87ms, despite ~40% fewer packets. Monitor reads are per-thread JVM
    /// work, not just round trips, so with no deep stacks to skip there is nothing to save and the monitor
    /// queries dominate. Ask for this mode because the lock graph is the answer you want — not on the
    /// assumption that it is always the cheaper dump.
    ///
    /// For a deadlock the lock graph *is* the answer and the stacks are only context. The holder of a
    /// contended lock is still named.
    ///
    /// Composes with `name_filter`, `only_suspended` and `limit`. `max_frames` and `package_filter` do
    /// nothing here, since no frames are read. Requires `monitors` (the default) — asking for neither
    /// locks nor stacks is refused rather than answered with empty rows.
    #[serde(default)]
    pub monitors_only: bool,
    /// Cap how long the VM may be held **suspended** while collecting, in milliseconds (default 2000;
    /// `0` = unbounded). Only meaningful with `suspend:true`.
    ///
    /// A dump is many round trips by construction, and the VM stays frozen for all of them — so the
    /// freeze grows with the thread count and frame depth, and on a remote JVM it is round-trip-latency
    /// bound rather than fast. When the budget runs out the dump resumes the VM immediately, returns what
    /// it gathered, and **says which threads it did not read** — a truncated dump is never presented as a
    /// complete one. Raise it for a deliberately deep dump, or narrow with `name_filter` / `limit` /
    /// `max_frames` / `package_filter` instead, which costs nothing.
    #[serde(default = "default_max_suspend_ms")]
    pub max_suspend_ms: u64,
    /// Suspend the VM for the duration of the dump, then resume it and verify it is running again.
    ///
    /// Off by default, and that default is the point: JDWP can only read a **suspended** thread's stack
    /// and locks, so a dump of a running VM reports every thread as unreadable — but silently pausing a
    /// shared instance to make the output look better is exactly the mistake SAFE-4 was about. Pass
    /// `true` to say explicitly "freeze it briefly, I accept that". A VM that is *already* suspended is
    /// read as it is and left suspended, whatever this is set to.
    #[serde(default)]
    pub suspend: bool,
}

/// Arguments for `debug.set_value`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetValueArgs {
    /// What to write. A local variable name (`counter`), a static field
    /// (`ConfigDefaultUtils.dsInfra` or a fully-qualified `pkg.Class.field`), an instance field
    /// reached from a suspended frame (`this.status`, `reserva.total`), or **one element** of an array,
    /// `List` or `Map` (`numbers[0]`, `tags[1]`, `counts["a"]`). A slice or filter target is refused —
    /// it names several elements, so there is nothing single to write. Accepts the legacy key `name`.
    #[serde(alias = "name")]
    pub target: String,
    /// Literal: int, 123L, true/false, null, or "string". Coerced to the target's declared type.
    pub value: String,
    /// Thread id (optional; defaults to last-hit thread). Needed for locals and instance fields.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Frame index (default 0). Used for locals and for resolving an instance-field prefix.
    #[serde(default)]
    pub frame_index: usize,
}

/// Arguments for `debug.set_exception_stop`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetExceptionBreakpointArgs {
    /// Exception class to break on (e.g. "java.lang.NullPointerException" or
    /// "br.com.infotravel.ErrorException"); its subclasses match too. Omit to break on ALL
    /// exceptions — noisy, since the JVM throws/catches internally constantly. The class must
    /// already be loaded (trigger it once if unsure).
    ///
    /// Takes a WILDCARD (`*.ValidationException`, `br.com.infotravel.*` — FILT-3) or a LIST
    /// (`["java.lang.IllegalStateException", "*.TimeoutException"]` — FILT-4). Both arm one `exc_…` per
    /// resolved class, because a JDWP exception request needs a concrete reference type — there is no
    /// `ClassMatch` for this event kind, which is also why none of them can be deferred. So a wildcard
    /// here matches only what is LOADED NOW: an exception class the JVM has not needed yet is invisible
    /// to it, and unlike a line breakpoint nothing will arm it later. `max_classes` bounds the expansion.
    #[serde(default)]
    pub class_pattern: Option<ClassPatterns>,
    /// Break on exceptions that ARE caught somewhere up the stack (default true).
    #[serde(default = "default_true")]
    pub caught: bool,
    /// Break on exceptions that are NOT caught (propagate out; default true).
    #[serde(default = "default_true")]
    pub uncaught: bool,
    /// Logpoint mode: on each throw, snapshot (throw location, thread, in-scope locals, exception
    /// type, catch location) into a ring buffer and resume immediately WITHOUT suspending — the safe
    /// choice on a shared instance, where a suspending exception breakpoint freezes other people's
    /// requests. Read snapshots with `debug.get_traces`.
    #[serde(default)]
    pub trace: bool,
    /// Only with `trace:true` — an expression to evaluate in the throwing frame and record alongside
    /// the snapshot (e.g. `this.getStatus()`).
    #[serde(default)]
    pub trace_expr: Option<String>,
    /// Only with `trace:true` — disarm automatically after this many hits (default 200; 0 = no limit),
    /// so a hot throw site can't flood the debuggee (TRACE-3).
    ///
    /// **This bound is load-bearing, not a formality.** Capture is serialised through the single JDWP
    /// connection and one event pump, so a traced stop point tops out at roughly **720 hits/s** with the
    /// default 3 caller frames, or **~1160/s** with `trace_frames: 0`. Past that the debugger — not the
    /// application — is the bottleneck, and every further hit queues behind the ones being captured. At
    /// the default 200 the exposure is a sub-second blip, which is most of the reason trace mode is safe
    /// to leave armed at all. **`0` on a site that fires thousands of times a second turns that blip into
    /// sustained throttling**: it is the single setting that removes the protection, so choose it
    /// knowingly. Loopback measurement against a trivial `WildFly` endpoint (#22) — the absolute figures
    /// move with hardware, the existence of a hits/s ceiling does not. **You do not have to trust those
    /// figures**: once the stop point has fired, `debug.list_stop_points` reports the mean capture measured
    /// on the JVM you are actually attached to — invert it for this ceiling — plus the rate hits are
    /// arriving at and the share of the window spent capturing (TRACE-7).
    #[serde(default)]
    pub trace_max_hits: Option<u32>,
    /// Only with `trace:true` — how many CALLER frames to record above the throw, so a swallowed
    /// exception says **which request path reached the catch**, which is usually the whole question
    /// (default 3; 0 for the throwing frame alone, capped at 20). Callers are recorded as
    /// `class.method:line` locations only — no locals, no invocation — so this stays safe in a
    /// read-only session. Each frame costs JVM round trips on every hit.
    ///
    /// The measured price of that depth: capture costs ~0.86ms per hit before any callers, and the
    /// default 3 frames add ~0.53ms on top (**+62%**), lowering the ceiling from ~1160 to ~720 hits/s.
    /// Kept at 3 regardless, because the chain is usually the answer rather than context — but
    /// `trace_frames: 0` is the cheap mode when the site is hot and you only need that it fired.
    /// Loopback measurement against a trivial `WildFly` endpoint (#22); `debug.list_stop_points` reports
    /// what the depth you chose is costing on *your* JVM once hits have landed.
    #[serde(default = "default_trace_frames")]
    pub trace_frames: usize,
    /// Only with `trace:true` — raise the per-value length cap on each capture, for a payload that does
    /// not fit the frugal defaults: a JSON body, a SOAP envelope, a `+=`-built SQL string. Unset keeps
    /// them exactly as they were — **100** characters for each in-scope local, **200** for the
    /// `trace_expr` result, the method-exit `returned` value and a watchpoint's old → new pair. Ceiling
    /// 4000; a larger request is clamped and the reply says so, and `0` is read as the ceiling rather
    /// than as "no limit" (which is what it means on `trace_max_hits`, and cannot mean here).
    ///
    /// **Truncation happens at CAPTURE time**, which is why this is an arming argument rather than a
    /// `debug.get_traces` one: the cut string is what the snapshot stores, so the remainder is gone and
    /// no later read can recover it. On a shared instance the workaround — suspend, then `debug.evaluate`
    /// with a large `max_result_length` — is precisely the thing trace mode exists to avoid.
    ///
    /// **The two defaults differ deliberately, and ONE argument raises both.** The locals are context,
    /// captured whether you asked for them or not, while `trace_expr` is the payload you named, so it
    /// already gets twice the room; both are frugal because a trace may fire hundreds of times into a
    /// bounded buffer. One knob rather than two because a caller raising the cap wants the payload and
    /// should not have to work out which of two numbers governs the value in front of them.
    ///
    /// It is still a bound. Buffer memory is roughly this cap × the hits recorded, and `trace_max_hits`
    /// defaults to 200 — so 4000 is ~800KB per captured value on one stop point at its default budget,
    /// which is where the ceiling comes from.
    #[serde(default = "default_trace_max_length")]
    pub trace_max_length: Option<usize>,
    /// Only report throws on this thread (hex id, e.g. `0x2a`). On a busy app server with hundreds of
    /// threads, restricting to your request thread is the single biggest noise reduction — get the id
    /// from `debug.list_threads {name_filter}` first, then arm, then trigger (FILT-1).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Most loaded classes ONE wildcard pattern may arm (default 20, clamped at 200 — FILT-3). Ignored
    /// for an exact class name. The reply says what a full expansion left out rather than truncating
    /// quietly.
    #[serde(default = "default_max_classes")]
    pub max_classes: usize,
}

/// Arguments for `debug.set_field_stop`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetWatchpointArgs {
    /// Class declaring the field (e.g. `ConfigDefaultUtils` or a fully-qualified
    /// `br.com.infotravel.util.ConfigDefaultUtils`). Must already be loaded — a watchpoint needs a
    /// concrete field id, so it can't be deferred like a line breakpoint.
    ///
    /// Takes a WILDCARD (`com.example.*` — FILT-3) or a LIST (FILT-4), arming one watch per matching
    /// loaded class that actually HAS the field: a class that matches the pattern but declares no such
    /// field is counted and reported, not treated as an error, since that is the expected majority for a
    /// broad pattern. `max_classes` bounds the expansion, and a watched field cannot be JIT-optimised —
    /// so a wildcard here de-optimises every class it matched, which is the reason to keep it narrow.
    pub class_name: ClassPatterns,
    /// Field to watch (e.g. `dsInfra`, `empresaId`). Inherited fields are found by walking
    /// superclasses; the watch is registered on the class that actually declares it.
    pub field_name: String,
    /// Break on writes (`FIELD_MODIFICATION`) — the default, and what answers "who mutates this?".
    #[serde(default = "default_true")]
    pub modify: bool,
    /// Also break on reads (`FIELD_ACCESS`). Noisy on a hot field; off by default.
    #[serde(default)]
    pub access: bool,
    /// Logpoint mode: on each hit, snapshot (mutating location, thread, in-scope locals, the field's
    /// old → new pair) into a ring buffer and resume immediately WITHOUT suspending — the safe choice
    /// on a shared instance. Read snapshots with `debug.get_traces`.
    #[serde(default)]
    pub trace: bool,
    /// Only with `trace:true` — an expression to evaluate in the mutating frame and record alongside
    /// the snapshot.
    #[serde(default)]
    pub trace_expr: Option<String>,
    /// Only with `trace:true` — disarm automatically after this many hits (default 200; 0 = no limit),
    /// so a hot field can't flood the debuggee (TRACE-3).
    ///
    /// **This bound is load-bearing, not a formality.** Capture is serialised through the single JDWP
    /// connection and one event pump, so a traced stop point tops out at roughly **720 hits/s** with the
    /// default 3 caller frames, or **~1160/s** with `trace_frames: 0`. Past that the debugger — not the
    /// application — is the bottleneck, and every further hit queues behind the ones being captured. At
    /// the default 200 the exposure is a sub-second blip, which is most of the reason trace mode is safe
    /// to leave armed at all. **`0` on a site that fires thousands of times a second turns that blip into
    /// sustained throttling**: it is the single setting that removes the protection, so choose it
    /// knowingly. Loopback measurement against a trivial `WildFly` endpoint (#22) — the absolute figures
    /// move with hardware, the existence of a hits/s ceiling does not. **You do not have to trust those
    /// figures**: once the stop point has fired, `debug.list_stop_points` reports the mean capture measured
    /// on the JVM you are actually attached to — invert it for this ceiling — plus the rate hits are
    /// arriving at and the share of the window spent capturing (TRACE-7).
    #[serde(default)]
    pub trace_max_hits: Option<u32>,
    /// Only with `trace:true` — how many CALLER frames to record above the mutating frame, so "who
    /// mutates this?" is answered with the path that got there, not just the innermost setter (default
    /// 3; 0 for the mutating frame alone, capped at 20). Callers are recorded as `class.method:line`
    /// locations only — no locals, no invocation — so this stays safe in a read-only session. Each
    /// frame costs JVM round trips on every hit.
    ///
    /// The measured price of that depth: capture costs ~0.86ms per hit before any callers, and the
    /// default 3 frames add ~0.53ms on top (**+62%**), lowering the ceiling from ~1160 to ~720 hits/s.
    /// Kept at 3 regardless, because the chain is usually the answer rather than context — but
    /// `trace_frames: 0` is the cheap mode when the site is hot and you only need that it fired.
    /// Loopback measurement against a trivial `WildFly` endpoint (#22); `debug.list_stop_points` reports
    /// what the depth you chose is costing on *your* JVM once hits have landed.
    #[serde(default = "default_trace_frames")]
    pub trace_frames: usize,
    /// Only with `trace:true` — raise the per-value length cap on each capture, for a payload that does
    /// not fit the frugal defaults: a JSON body, a SOAP envelope, a `+=`-built SQL string. Unset keeps
    /// them exactly as they were — **100** characters for each in-scope local, **200** for the
    /// `trace_expr` result, the method-exit `returned` value and a watchpoint's old → new pair. Ceiling
    /// 4000; a larger request is clamped and the reply says so, and `0` is read as the ceiling rather
    /// than as "no limit" (which is what it means on `trace_max_hits`, and cannot mean here).
    ///
    /// **Truncation happens at CAPTURE time**, which is why this is an arming argument rather than a
    /// `debug.get_traces` one: the cut string is what the snapshot stores, so the remainder is gone and
    /// no later read can recover it. On a shared instance the workaround — suspend, then `debug.evaluate`
    /// with a large `max_result_length` — is precisely the thing trace mode exists to avoid.
    ///
    /// **The two defaults differ deliberately, and ONE argument raises both.** The locals are context,
    /// captured whether you asked for them or not, while `trace_expr` is the payload you named, so it
    /// already gets twice the room; both are frugal because a trace may fire hundreds of times into a
    /// bounded buffer. One knob rather than two because a caller raising the cap wants the payload and
    /// should not have to work out which of two numbers governs the value in front of them.
    ///
    /// It is still a bound. Buffer memory is roughly this cap × the hits recorded, and `trace_max_hits`
    /// defaults to 200 — so 4000 is ~800KB per captured value on one stop point at its default budget,
    /// which is where the ceiling comes from.
    #[serde(default = "default_trace_max_length")]
    pub trace_max_length: Option<usize>,
    /// Only report touches from this thread (hex id, e.g. `0x2a`). On a busy app server, restricting
    /// to your request thread is the single biggest noise reduction — get the id from
    /// `debug.list_threads {name_filter}` first, then arm, then trigger (FILT-1).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Most loaded classes ONE wildcard pattern may arm (default 20, clamped at 200 — FILT-3). Ignored
    /// for an exact class name. Watches de-optimise every class they are armed on, so this cap is doing
    /// more work here than for a line breakpoint.
    #[serde(default = "default_max_classes")]
    pub max_classes: usize,
}

/// Arguments for `debug.set_method_exit_stop` (METH-1).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetMethodBreakpointArgs {
    /// Class whose method returns you want to see (e.g. `br.com.infotravel.IntegraSrv`), optionally
    /// with a leading/trailing `*`. This is a JDWP `ClassMatch`, so it fires for **every method** of
    /// every matching class — give `method` as well unless you really want all of them.
    ///
    /// Also takes a LIST (`["*.OrderService", "*.PaymentService"]` — FILT-4), arming one `mexit_…` per
    /// pattern. Unlike the other stop points a wildcard costs nothing extra here and needs no expansion:
    /// the JVM does the matching, so one request covers every class the pattern matches, including classes
    /// that load later. That is why this tool had pattern support from the start and the others did not.
    pub class_pattern: ClassPatterns,
    /// Only report returns from this method name. Filtered on our side, because JDWP has no
    /// method-name modifier — the JVM still reports every method of the class and non-matching exits
    /// are dropped here. Overloads all match, since the name is all JDWP gives us to compare.
    #[serde(default)]
    pub method: Option<String>,
    /// Logpoint mode: snapshot each return (location, thread, in-scope locals, the returned value) and
    /// resume immediately WITHOUT suspending. **Defaults to true, unlike every other stop point** — a
    /// suspending method exit on a hot method is the fastest way to freeze a shared JVM this tool
    /// offers. Setting it false needs a concrete class and a `method`, or it is refused.
    #[serde(default = "default_true")]
    pub trace: bool,
    /// Only with `trace:true` — an expression evaluated in the returning frame and recorded alongside
    /// the snapshot.
    #[serde(default)]
    pub trace_expr: Option<String>,
    /// Only with `trace:true` — disarm automatically after this many hits (default 200; 0 = no limit).
    /// Method exits are the noisiest event in JDWP, so this budget matters more here than anywhere else.
    ///
    /// **This bound is load-bearing, not a formality.** Capture is serialised through the single JDWP
    /// connection and one event pump, so a traced stop point tops out at roughly **720 hits/s** with the
    /// default 3 caller frames, or **~1160/s** with `trace_frames: 0`. Past that the debugger — not the
    /// application — is the bottleneck, and every further hit queues behind the ones being captured. At
    /// the default 200 the exposure is a sub-second blip, which is most of the reason trace mode is safe
    /// to leave armed at all. **`0` on a site that fires thousands of times a second turns that blip into
    /// sustained throttling**: it is the single setting that removes the protection, so choose it
    /// knowingly. Loopback measurement against a trivial `WildFly` endpoint (#22) — the absolute figures
    /// move with hardware, the existence of a hits/s ceiling does not. **You do not have to trust those
    /// figures**: once the stop point has fired, `debug.list_stop_points` reports the mean capture measured
    /// on the JVM you are actually attached to — invert it for this ceiling — plus the rate hits are
    /// arriving at and the share of the window spent capturing (TRACE-7).
    #[serde(default)]
    pub trace_max_hits: Option<u32>,
    /// Only with `trace:true` — how many caller frames to record above the return, as
    /// `class.method:line` (default 3; 0 for the returning frame alone, capped at 20).
    ///
    /// The measured price of that depth: capture costs ~0.86ms per hit before any callers, and the
    /// default 3 frames add ~0.53ms on top (**+62%**), lowering the ceiling from ~1160 to ~720 hits/s.
    /// Kept at 3 regardless, because the chain is usually the answer rather than context — but
    /// `trace_frames: 0` is the cheap mode when the site is hot and you only need that it fired.
    /// Loopback measurement against a trivial `WildFly` endpoint (#22); `debug.list_stop_points` reports
    /// what the depth you chose is costing on *your* JVM once hits have landed.
    #[serde(default = "default_trace_frames")]
    pub trace_frames: usize,
    /// Only with `trace:true` — raise the per-value length cap on each capture, for a payload that does
    /// not fit the frugal defaults: a JSON body, a SOAP envelope, a `+=`-built SQL string. Unset keeps
    /// them exactly as they were — **100** characters for each in-scope local, **200** for the
    /// `trace_expr` result, the method-exit `returned` value and a watchpoint's old → new pair. Ceiling
    /// 4000; a larger request is clamped and the reply says so, and `0` is read as the ceiling rather
    /// than as "no limit" (which is what it means on `trace_max_hits`, and cannot mean here).
    ///
    /// **Truncation happens at CAPTURE time**, which is why this is an arming argument rather than a
    /// `debug.get_traces` one: the cut string is what the snapshot stores, so the remainder is gone and
    /// no later read can recover it. On a shared instance the workaround — suspend, then `debug.evaluate`
    /// with a large `max_result_length` — is precisely the thing trace mode exists to avoid.
    ///
    /// **The two defaults differ deliberately, and ONE argument raises both.** The locals are context,
    /// captured whether you asked for them or not, while `trace_expr` is the payload you named, so it
    /// already gets twice the room; both are frugal because a trace may fire hundreds of times into a
    /// bounded buffer. One knob rather than two because a caller raising the cap wants the payload and
    /// should not have to work out which of two numbers governs the value in front of them.
    ///
    /// It is still a bound. Buffer memory is roughly this cap × the hits recorded, and `trace_max_hits`
    /// defaults to 200 — so 4000 is ~800KB per captured value on one stop point at its default budget,
    /// which is where the ceiling comes from.
    #[serde(default = "default_trace_max_length")]
    pub trace_max_length: Option<usize>,
    /// Only report returns on this thread (hex id, e.g. `0x2a`). On a busy app server this is the
    /// single biggest noise reduction for this event kind — get the id from `debug.list_threads`
    /// or `debug.thread_dump` first.
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Arguments for `debug.force_return`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForceReturnArgs {
    /// Return value literal, coerced to the method's declared return type: int, 123L, true/false,
    /// null, or "string". Omit (or pass "void") for a void method.
    #[serde(default)]
    pub value: Option<String>,
    /// Thread id (optional; defaults to last-hit thread). Must be suspended.
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Arguments for `debug.reload_class` (SWAP-1).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReloadClassArgs {
    /// Fully-qualified class name, e.g. com.example.OrderService. Must already be LOADED — a class
    /// the JVM has never loaded has nothing to redefine, and loading it is the application's job.
    pub class_name: String,
    /// Read the new bytecode from exactly this `.class` file, bypassing `class_roots`. For a build
    /// output that is not laid out as a package tree, or a one-off.
    #[serde(default)]
    pub class_file: Option<String>,
    /// Directories to search for this call only, replacing the session's class roots (set at
    /// `debug.attach` or by `JDWP_CLASS_ROOTS`).
    #[serde(default)]
    pub class_roots: Option<Vec<String>>,
    /// Report what WOULD be shipped — the resolved file, its size, and whether the JVM can `HotSwap` at
    /// all — and send nothing. The safe first call against a shared JVM.
    #[serde(default)]
    pub dry_run: bool,
    /// Thread to check for frames still running the old bytecode after a successful swap (defaults to
    /// the last-hit thread). A redefinition does not touch frames already on the stack, so this is how
    /// the reply can tell you that the method you just changed is one of them.
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Arguments for `debug.check_stale` (DISC-7).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckStaleArgs {
    /// Fully-qualified class name, e.g. com.example.OrderService. Must already be loaded.
    pub class_name: String,
    /// Compare against exactly this `.class` file, bypassing `class_roots`.
    #[serde(default)]
    pub class_file: Option<String>,
    /// Directories to search for this call only, replacing the session's class roots (set at
    /// `debug.attach` or by `JDWP_CLASS_ROOTS`).
    #[serde(default)]
    pub class_roots: Option<Vec<String>>,
    /// Max drifting methods to name; the rest are reported as a count.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Also compare each method's **bytecode**, not just its line table (DISC-9).
    ///
    /// Off by default because it doubles the JDWP cost — one `Method.Bytecodes` per method on top of the
    /// one `Method.LineTable` — and the line table already catches the edit that hurts most, a moved
    /// line. Turn it on for the edit the line table cannot see at all: one that changes a body without
    /// moving any line (`<` to `<=`, a changed constant, a swapped operator), which is also the
    /// commonest edit in a compile-and-retest loop.
    ///
    /// It is the only evidence that works on a `-g:none` build, which has code and no line numbers.
    #[serde(default)]
    pub bytecode: bool,
}

/// Arguments for `debug.pop_frame` (SWAP-1).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PopFrameArgs {
    /// Frame index to pop, as numbered by `debug.get_stack` (0 = innermost). Every frame above it goes
    /// too — that is what JDWP does, not a convenience added here.
    #[serde(default)]
    pub frame: usize,
    /// Thread id (optional; defaults to last-hit thread). Must be suspended.
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression guard: every arg struct produces a valid object schema. Catches derive breakage.
    #[test]
    fn all_schemas_generate() {
        let schemas = [
            serde_json::to_value(schemars::schema_for!(AttachArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ClearBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(StepArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(GetStackArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(EvaluateArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ListThreadsArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetValueArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetExceptionBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetWatchpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ForceReturnArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(GetTracesArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(GetLastEventArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ToggleBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ThreadDumpArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetMethodBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ReloadClassArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(PopFrameArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(LaunchArgs)).unwrap(),
        ];
        for s in schemas {
            assert_eq!(s.get("type").and_then(|t| t.as_str()), Some("object"));
        }
    }

    // LAUNCH-1: the two defaults that decide what this tool is FOR. `suspend` is the whole reason it exists
    // (breaking before the program's first instruction), and terminating on disconnect is the lifetime policy
    // the tool description promises — both are quoted there, so a silent flip would make the docs lie.
    #[test]
    fn launch_defaults_to_suspended_and_to_owning_the_process() {
        let a: LaunchArgs = serde_json::from_value(serde_json::json!({"main_class": "M"})).unwrap();
        assert!(a.suspend, "suspend=y is the point of launching rather than attaching");
        assert!(!a.detach_on_disconnect, "the JVM we started dies with the session unless asked otherwise");
        assert_eq!(a.port, 0, "0 means pick a free port");
        assert!(!a.read_only);
    }

    // FILT-4: the four arming tools must accept a class argument written either way, because the whole
    // point is that nothing written before it has to change.
    #[test]
    fn a_class_argument_deserialises_from_a_string_or_a_list() {
        let one: SetBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": "com.example.Order", "line": 1}))
                .unwrap();
        assert_eq!(one.class_pattern.list(), vec!["com.example.Order".to_string()]);

        let many: SetBreakpointArgs = serde_json::from_value(serde_json::json!({
            "class_pattern": ["com.example.Order", "com.example.*Repo"],
            "method": "handle"
        }))
        .unwrap();
        assert_eq!(
            many.class_pattern.list(),
            vec!["com.example.Order".to_string(), "com.example.*Repo".to_string()]
        );

        // Every arming tool, including the one whose argument is called `class_name`.
        let exc: SetExceptionBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": ["A", "B"]})).unwrap();
        assert_eq!(exc.class_pattern.unwrap().list().len(), 2);
        let watch: SetWatchpointArgs =
            serde_json::from_value(serde_json::json!({"class_name": ["A", "B"], "field_name": "f"})).unwrap();
        assert_eq!(watch.class_name.list().len(), 2);
        let mexit: SetMethodBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": ["*.A", "*.B"]})).unwrap();
        assert_eq!(mexit.class_pattern.list().len(), 2);
    }

    // Blanks and repeats are dropped rather than armed: a repeated pattern would put two stop points on
    // one location, which is never meant and otherwise costs JDWP requests to discover.
    #[test]
    fn a_class_list_is_trimmed_and_deduped_in_order() {
        let a: SetBreakpointArgs = serde_json::from_value(serde_json::json!({
            "class_pattern": ["  com.example.B  ", "com.example.A", "", "com.example.B", "   "],
            "method": "handle"
        }))
        .unwrap();
        assert_eq!(
            a.class_pattern.list(),
            vec!["com.example.B".to_string(), "com.example.A".to_string()],
            "order is the caller's, duplicates and blanks are gone"
        );
    }

    // FILT-3: the expansion cap is what stops one mistyped wildcard arming thousands of stop points on a
    // shared JVM, and the tool descriptions quote both numbers.
    #[test]
    fn max_classes_defaults_to_twenty_and_is_documented() {
        let bp: SetBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": "C", "line": 1})).unwrap();
        assert_eq!(bp.max_classes, DEFAULT_MAX_CLASSES);
        assert_eq!(DEFAULT_MAX_CLASSES, 20);
        assert_eq!(MAX_CLASSES_CEILING, 200);

        let asked: SetBreakpointArgs = serde_json::from_value(
            serde_json::json!({"class_pattern": "C*", "method": "m", "max_classes": 5}),
        )
        .unwrap();
        assert_eq!(asked.max_classes, 5);
    }

    // The deep-expansion knobs default to off/2/16 — the tool descriptions state those numbers, and
    // a silent change would make the docs wrong rather than break a test.
    #[test]
    fn expansion_defaults_match_the_documented_values() {
        let ev: EvaluateArgs = serde_json::from_value(serde_json::json!({"expression": "x"})).unwrap();
        assert!(!ev.expand_objects, "expansion must be opt-in: it invokes methods in the debuggee");
        assert_eq!(ev.max_depth, 2);
        assert_eq!(ev.max_children, 16);

        let st: GetStackArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!st.expand_objects);
        assert_eq!(st.max_depth, 2);
        assert_eq!(st.max_children, 16);
    }

    // TRACE-5: all three traced stop points default to the SAME caller depth, and it is the documented
    // one. A per-tool default would make "which path reached this?" depend on which tool you reached
    // for, and the tool descriptions state the number.
    #[test]
    fn trace_frames_defaults_are_shared_and_documented() {
        let bp: SetBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": "C", "line": 1})).unwrap();
        let exc: SetExceptionBreakpointArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        let watch: SetWatchpointArgs =
            serde_json::from_value(serde_json::json!({"class_name": "C", "field_name": "f"})).unwrap();

        assert_eq!(bp.trace_frames, 3);
        assert_eq!(exc.trace_frames, bp.trace_frames);
        assert_eq!(watch.trace_frames, bp.trace_frames);

        // Explicit 0 must survive deserialization as 0, not fall back to the default — it is how a
        // caller asks for the original one-frame snapshot.
        let off: SetBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": "C", "line": 1, "trace_frames": 0}))
                .unwrap();
        assert_eq!(off.trace_frames, 0);
    }

    // TRACE-9: the same shape for `trace_max_length`, and one thing more that `trace_frames` does not
    // have to prove — that UNSET is distinguishable from any number. The two caps it raises differ
    // (100 for a local, 200 for the trace_expr result), so a default that was a number would silently
    // change one of them for every caller who never asked; `None` is what keeps today's output
    // byte-identical, and this test is what stops it becoming `0` or `100` in a later tidy-up.
    #[test]
    fn trace_max_length_defaults_to_unset_on_all_four_arming_tools() {
        let bp: SetBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": "C", "line": 1})).unwrap();
        let exc: SetExceptionBreakpointArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        let watch: SetWatchpointArgs =
            serde_json::from_value(serde_json::json!({"class_name": "C", "field_name": "f"})).unwrap();
        let mexit: SetMethodBreakpointArgs =
            serde_json::from_value(serde_json::json!({"class_pattern": "C"})).unwrap();

        assert_eq!(bp.trace_max_length, None, "unset must not be a number");
        assert_eq!(exc.trace_max_length, bp.trace_max_length);
        assert_eq!(watch.trace_max_length, bp.trace_max_length);
        assert_eq!(mexit.trace_max_length, bp.trace_max_length);

        // A value the caller did give survives deserialization as itself, on all four.
        let raised: SetBreakpointArgs = serde_json::from_value(
            serde_json::json!({"class_pattern": "C", "line": 1, "trace_max_length": 3000}),
        )
        .unwrap();
        assert_eq!(raised.trace_max_length, Some(3000));

        // And `0` reaches the handler as `Some(0)` rather than collapsing into "unset" — the clamp is
        // what decides its meaning, and it cannot decide anything it was never told.
        let zero: SetBreakpointArgs = serde_json::from_value(
            serde_json::json!({"class_pattern": "C", "line": 1, "trace_max_length": 0}),
        )
        .unwrap();
        assert_eq!(zero.trace_max_length, Some(0));
    }

    // `get_last_event` gained a buffer (EVT-1) but must stay backward compatible: a bare call still
    // returns exactly the newest event and does not consume it, so polling it twice is safe.
    #[test]
    fn get_last_event_defaults_to_the_newest_event_only_and_keeps_it() {
        let a: GetLastEventArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(a.limit, 1);
        assert!(!a.drain);
    }

    // set_value's target accepts both the new `target` key and the legacy `name` key, so existing
    // callers keep working after the locals→fields generalization.
    #[test]
    fn set_value_accepts_target_and_legacy_name() {
        let by_target: SetValueArgs = serde_json::from_value(
            serde_json::json!({"target": "ConfigDefaultUtils.dsInfra", "value": "DEV"}),
        )
        .unwrap();
        assert_eq!(by_target.target, "ConfigDefaultUtils.dsInfra");

        let by_name: SetValueArgs =
            serde_json::from_value(serde_json::json!({"name": "counter", "value": "5"})).unwrap();
        assert_eq!(by_name.target, "counter");
    }
}
