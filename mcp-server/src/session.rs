// Debug session management
//
// Manages JDWP connection state, breakpoints, and thread tracking

use jdwp_client::{EventSet, JdwpConnection};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub type SessionId = String;

#[derive(Debug)]
pub struct DebugSession {
    pub connection: JdwpConnection,
    /// The `host:port` this session attached to. Kept so `debug.list_sessions` can identify a session
    /// by where it points rather than by an opaque id — the connection itself doesn't remember.
    pub endpoint: String,
    pub breakpoints: HashMap<String, BreakpointInfo>,
    /// Ring buffer of reportable events, oldest first. Bounded by `MAX_EVENTS`.
    ///
    /// A single `Option` slot here used to mean a second hit erased the first with no trace — the
    /// worst kind of gap in a debugging tool, because the answer you read looks complete. Traces got
    /// a bounded buffer from the start; events now get the same treatment.
    pub events: VecDeque<EventRecord>,
    /// Monotonic sequence for event records (survives ring-buffer eviction).
    pub event_seq: u64,
    /// How many events the ring buffer has evicted — reported so a caller knows it fell behind.
    pub events_dropped: u64,
    pub event_listener_task: Option<JoinHandle<()>>,
    /// Thread of the most recent suspension event — used to default `thread_id`.
    pub last_thread: Option<u64>,
    /// Active single-step request id (must be cleared before the next resume).
    pub pending_step: Option<i32>,
    /// When the VM last suspended; cleared on resume. Drives the watchdog.
    pub suspended_since: Option<std::time::Instant>,
    /// **Why** the VM is suspended, recorded at suspension time and cleared on resume.
    ///
    /// The watchdog used to re-derive the offending stop point from the newest buffered event, which
    /// `get_last_event {drain:true}` could erase — so the polling caller `drain` exists for was exactly
    /// the one whose freeze never got disarmed (SAFE-5). One authoritative field instead of two sources
    /// of truth, and it also lets a manual `debug.pause` be told apart from a stop-point hit (SAFE-4).
    pub suspended_cause: Option<SuspendCause>,
    pub watchdog_task: Option<JoinHandle<()>>,
    /// What the watchdog last did, if anything — surfaced in `list_stop_points` and `get_last_event`
    /// so a caller who was away learns the VM was auto-resumed and which stop point was disarmed (SAFE-2).
    pub last_watchdog_note: Option<String>,
    /// Traced stop points that disarmed themselves on reaching their hit budget (TRACE-3), as
    /// `(note, times)` keyed by note text. Surfaced by `get_traces` so silence is never mistaken for
    /// "no more hits"; cleared with `clear`.
    ///
    /// Repeats are **collapsed** rather than appended, and the map is capped (`MAX_TRACE_DISARMS`).
    /// It was an unbounded `Vec`, which only looked harmless while an auto-disarm also deleted the stop
    /// point: BP-2/BP-3 made re-arming easy, so one budgeted logpoint can now disarm over and over. Every
    /// other buffer here is bounded, and "`watch_3` disarmed itself 12 times" beats identical lines
    /// anyway (SAFE-8).
    pub trace_disarms: std::collections::BTreeMap<String, u32>,
    /// How many distinct disarm notes were dropped because the map was full — reported, like
    /// `events_dropped`, so a full buffer never reads as a quiet one.
    pub trace_disarms_dropped: u64,
    /// Read-only guard (SAFE-3): when set, method invocation, `set_value` and `force_return` are
    /// refused, so pointing the debugger at a production JVM can't accidentally mutate it. A guard
    /// against accident, NOT a security boundary — anyone who can reach the JDWP port can do anything.
    pub read_only: bool,
    /// Directories `debug.source` searches for a class's `.java` file (DISC-3), resolved once at
    /// attach from the tool argument or `JDWP_SOURCE_ROOTS`.
    ///
    /// Per session rather than per call because a checkout belongs to the JVM you attached to, not to
    /// the question you are asking: the same list would otherwise be repeated on every call, and two
    /// sessions against different deployments need different lists.
    pub source_roots: Vec<std::path::PathBuf>,
    /// Breakpoints requested on classes not yet loaded. Each holds a `CLASS_PREPARE` request that
    /// fires when the class loads; the event pump then arms the real breakpoint. See handlers.rs.
    pub pending_breakpoints: Vec<PendingBreakpoint>,
    /// Active exception breakpoints (EXCEPTION event requests), keyed by their `exc_` id.
    pub exception_requests: HashMap<String, ExceptionRequestInfo>,
    /// Active field watchpoints (`FIELD_ACCESS` / `FIELD_MODIFICATION` requests), keyed by `watch_` id.
    pub watchpoints: HashMap<String, WatchpointInfo>,
    /// Active method-exit requests (METH-1), keyed by their `mexit_` id.
    pub method_exits: HashMap<String, MethodExitRequestInfo>,
    /// Ring buffer of trace/logpoint snapshots (see `TraceRecord`). Bounded by `MAX_TRACES`.
    pub traces: VecDeque<TraceRecord>,
    /// Monotonic sequence for trace records (survives ring-buffer eviction).
    pub trace_seq: u64,
    /// Monotonic counter behind caller-facing stop-point ids (`bp_`/`exc_`/`watch_`).
    ///
    /// Ids used to embed the JDWP request id, so re-arming a disabled stop point gave it a *new* id and
    /// silently broke any id the caller had stored — the thing that made `toggle_stop_point` awkward to
    /// script (BP-3). The request id is an internal detail now: still reported, never the identity.
    pub stop_seq: u64,
    /// Push channel to the MCP client (EVT-2). Lives on the session because the two things that need
    /// it — the event pump and the watchdog — already hold a session and nothing else.
    ///
    /// It never replaces the `events` buffer above. A notification is best-effort and a client may
    /// never read one, so `debug.get_last_event` has to remain sufficient on its own.
    pub alerter: crate::protocol::Alerter,
}

/// Max trace snapshots retained per session; oldest are evicted (documented cap for TRACE-1).
pub const MAX_TRACES: usize = 500;

/// Max distinct self-disarm notes retained per session (SAFE-8). Small on purpose: these are notices to
/// act on, not a log, and repeats of the same one are collapsed into a count rather than taking a slot.
pub const MAX_TRACE_DISARMS: usize = 32;

/// Max reportable events retained per session; oldest are evicted, counted in `events_dropped`.
///
/// Smaller than `MAX_TRACES` on purpose: a suspending event holds a thread, so they arrive at human
/// pace, whereas traces stream from a running VM. 100 is far more than a session can work through.
pub const MAX_EVENTS: usize = 100;

/// One reportable event as it arrived, with a sequence number so `debug.get_last_event` can say
/// which of several hits it is showing.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub seq: u64,
    pub set: EventSet,
}

/// Why the VM is currently suspended — what the watchdog needs to act correctly on a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendCause {
    /// A stop point suspended it, carrying the JDWP request id that fired — the thing to disarm so the
    /// VM isn't re-frozen on the very next hit (SAFE-2).
    StopPoint(i32),
    /// `debug.pause` suspended every thread by hand. There is **no** stop point to disarm, so a
    /// watchdog resume here must not claim it failed to identify one (SAFE-4).
    ManualPause,
}

impl DebugSession {
    /// Push a reportable event, evicting the oldest if the buffer is full. Returns the assigned seq.
    pub fn push_event(&mut self, set: EventSet) -> u64 {
        self.event_seq += 1;
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
            self.events_dropped += 1;
        }
        self.events.push_back(EventRecord { seq: self.event_seq, set });
        self.event_seq
    }

    /// Record that the VM is now suspended, and why. Paired with [`mark_resumed`](Self::mark_resumed) so
    /// the timestamp and the cause can't drift apart — the bug SAFE-5 fixed came from tracking them
    /// separately.
    pub fn mark_suspended(&mut self, cause: SuspendCause) {
        self.suspended_since = Some(std::time::Instant::now());
        self.suspended_cause = Some(cause);
    }

    /// Record that the VM is running again. Every resume path calls this, so neither field is left stale.
    pub fn mark_resumed(&mut self) {
        self.suspended_since = None;
        self.suspended_cause = None;
    }

    /// Record that a traced stop point disarmed itself (SAFE-8). Repeats of the same note increment a
    /// count instead of adding an entry, and once `MAX_TRACE_DISARMS` distinct notes are held a new one
    /// is dropped and counted rather than growing the map without bound.
    pub fn note_trace_disarm(&mut self, note: String) {
        if let Some(n) = self.trace_disarms.get_mut(&note) {
            *n += 1;
        } else if self.trace_disarms.len() < MAX_TRACE_DISARMS {
            self.trace_disarms.insert(note, 1);
        } else {
            self.trace_disarms_dropped += 1;
        }
    }

    /// Allocate the next caller-facing stop-point id, e.g. `next_stop_id("bp_")` → `bp_1`.
    ///
    /// Stable for the life of the stop point, so disabling and re-arming it keeps the id the caller
    /// already has (BP-3).
    pub fn next_stop_id(&mut self, prefix: &str) -> String {
        self.stop_seq += 1;
        format!("{prefix}{}", self.stop_seq)
    }
}

/// What a traced stop point has actually cost, measured hit by hit (TRACE-7).
///
/// #22 established the *typical* price of a traced hit — ~0.86 ms plus ~0.53 ms for the caller chain,
/// so roughly 720 hits/s — from one measurement, on one machine, against one endpoint. Those figures are
/// in the tool descriptions, and they are the best a document can do. What a caller actually needs to
/// know is what **this** stop point, on **their** site, is costing right now, and the debugger is the
/// only thing that can answer that: it already counts hits for `trace_max_hits`, so it only lacked the
/// clock. Same move #17 made for `thread_dump`, which used to report its packet count and now reports the
/// duration it held the VM.
///
/// Only the **capture** window is timed — the snapshot and caller-chain read — not the whole event-pump
/// iteration. Our own bookkeeping (budget arithmetic, the resume, this struct) must not inflate the
/// number we then report as the cost of tracing.
#[derive(Debug, Clone, Default)]
pub struct TraceCost {
    /// Captures recorded. Hits dropped by a condition or a method filter are **not** counted, for the
    /// same reason they don't charge the budget: nothing was captured, so nothing was paid.
    pub captures: u64,
    /// Summed capture windows.
    pub total: std::time::Duration,
    /// When the first and most recent capture began. The gap between them is the observation window, and
    /// it is what separates "fires constantly" from "fired twice an hour ago".
    first: Option<std::time::Instant>,
    last: Option<std::time::Instant>,
}

impl TraceCost {
    /// Record one capture that began at `started` and took `took`.
    pub fn record(&mut self, started: std::time::Instant, took: std::time::Duration) {
        self.captures += 1;
        self.total = self.total.saturating_add(took);
        self.first.get_or_insert(started);
        self.last = Some(started);
    }

    /// Mean cost of one capture, or `None` before anything has been captured.
    pub fn mean_capture(&self) -> Option<std::time::Duration> {
        (self.captures > 0).then(|| self.total / u32::try_from(self.captures).unwrap_or(u32::MAX))
    }

    // There was a `sustainable_rate` here — 1/mean, the rate past which hits queue, reported as
    // `sustains ~608 hit(s)/s`. Removed deliberately: it is a re-expression of `mean_capture`, so it added
    // a figure to the line without adding information, and every reader had to be told which of two
    // "rates" they were looking at. The mean is the primitive; anyone comparing against #22's documented
    // ~720 hits/s can invert it. `observed_rate` below is the one that cannot be derived from anything
    // else on the line, and `capture_share` is what it is for.

    /// Hits per second actually **arriving**, over the window from the first capture to the last.
    ///
    /// `None` until there are two captures: one hit establishes no interval, and dividing by a window of
    /// zero would report an infinite rate for the quietest possible trace.
    #[allow(clippy::cast_precision_loss)] // as above
    pub fn observed_rate(&self) -> Option<f64> {
        let (first, last) = (self.first?, self.last?);
        let window = last.duration_since(first).as_secs_f64();
        // N captures span N-1 intervals; using N would inflate the rate of an infrequent trace.
        (self.captures >= 2 && window > 0.0).then(|| (self.captures - 1) as f64 / window)
    }

    /// The fraction of the observation window spent capturing — arrival rate × cost per hit.
    ///
    /// This is the number that answers "is this trace hurting the instance?", which neither figure does
    /// alone: a cheap capture on a hot line and an expensive one on a quiet line can cost the same.
    pub fn capture_share(&self) -> Option<f64> {
        Some(self.observed_rate()? * self.mean_capture()?.as_secs_f64())
    }
}

/// One captured hit of a trace/logpoint breakpoint: where it fired, on which thread, the in-scope
/// locals/args at that point, and an optional evaluated expression. Recorded without leaving the
/// thread suspended.
#[derive(Debug, Clone)]
pub struct TraceRecord {
    pub seq: u64,
    pub bp_id: String,
    pub thread: u64,
    pub class: String,
    pub method: String,
    pub line: Option<i32>,
    /// (name, rendered value) for each in-scope local/argument at the hit.
    pub args: Vec<(String, String)>,
    /// The calling chain above the hit frame, nearest caller first, each as `class.method:line`
    /// (TRACE-5). Empty when `trace_frames` was 0, or when the hit is already the outermost frame.
    ///
    /// **Locations only, deliberately.** The hit frame's locals are the payload; the callers are
    /// context, and reading every frame's variable table would multiply the per-hit cost on a logpoint
    /// that may fire hundreds of times. It also keeps the whole capture invocation-free, so caller
    /// chains work in a read-only session (SAFE-6) — unlike object expansion.
    pub callers: Vec<String>,
    /// (expression, rendered result) when the logpoint had a trace expression.
    pub expr: Option<(String, String)>,
    /// What kind of stop point this came from, and anything specific to it: for an exception, the
    /// type and catch location; for a watchpoint, the field and its old → new pair. Empty for a
    /// plain line logpoint, whose location and args already say everything.
    ///
    /// Kept as ordered key/value pairs rather than a formatted string so the renderer, not the
    /// capture, decides how a trace line reads.
    pub detail: Vec<(String, String)>,
}

/// An active exception breakpoint: an EXCEPTION event request that fires when a matching
/// exception is thrown. Tracked so it shows in `list_stop_points` and is cleared by
/// `clear_stop_point` / panic, like a normal breakpoint.
#[derive(Debug, Clone)]
// Four bools, and each is an independent property of the JDWP request as the protocol defines it
// (armed / caught / uncaught / traced) rather than a parameter bag that wants splitting up.
#[allow(clippy::struct_excessive_bools)]
pub struct ExceptionRequestInfo {
    /// The `exc_` id reported to the caller.
    pub id: String,
    /// The live JDWP EXCEPTION event-request id, or `None` while disabled (BP-2): an auto-disarm keeps
    /// the definition — notably `trace_expr` — so it can be re-armed without retyping it.
    pub request_id: Option<i32>,
    /// Whether this request is currently armed in the JVM.
    pub enabled: bool,
    /// The resolved exception ref type, kept so a disabled request can be re-armed (BP-2). `None` means
    /// "all exceptions", which is how it was registered.
    pub ref_type: Option<u64>,
    /// Dotted class pattern the caller gave, or "*" for all exceptions.
    pub class_pattern: String,
    pub caught: bool,
    pub uncaught: bool,
    /// Non-suspending trace mode: armed with `EventThread`, each throw is snapshotted into the trace
    /// ring buffer and the hit thread resumed, so a shared JVM is never frozen (TRACE-2).
    pub trace: bool,
    /// Optional expression evaluated in the throwing frame and recorded on each trace hit.
    pub trace_expr: Option<String>,
    /// Remaining trace-hit budget (TRACE-3): each traced hit decrements it, and on reaching zero the
    /// request disarms itself so a hot throw site can't flood. `None` means unbounded.
    pub trace_budget: Option<u32>,
    /// How many caller frames each traced throw records above the throwing frame (TRACE-5).
    pub trace_frames: usize,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    /// Thread this request is filtered to (`ThreadOnly`), if any — for the `list_stop_points` line (FILT-1).
    pub thread_filter: Option<u64>,
}

/// An active field watchpoint: a `FIELD_ACCESS` or `FIELD_MODIFICATION` event request on one field.
/// Tracked so it shows in `list_stop_points` and is cleared by `clear_stop_point` / panic like a
/// normal breakpoint — `ClearAllBreakpoints` does not touch it.
#[derive(Debug, Clone)]
pub struct WatchpointInfo {
    /// The live JDWP event-request id, or `None` while disabled (BP-2).
    pub request_id: Option<i32>,
    /// Whether this watch is currently armed in the JVM.
    pub enabled: bool,
    /// The declaring type and field id, kept **only** so a disabled watch can be re-armed (BP-2).
    ///
    /// Reporting a hit deliberately does not use these — a hit carries its own declaring type and field,
    /// so `get_last_event` can still describe a hit whose watchpoint has already been cleared.
    pub arm: (u64, u64),
    /// Which event kind this was registered as — `Clear` needs the same kind back.
    pub kind: jdwp_client::WatchKind,
    /// Dotted class name the caller gave, for messages.
    pub class_name: String,
    pub field_name: String,
    /// Whether the field is static, for the `list_stop_points` line.
    ///
    /// Hit *reporting* deliberately does not read the declaring type or field id from here (see `arm`):
    /// a hit carries all of it, so `get_last_event` resolves them from the event and can still describe
    /// a hit whose watchpoint has already been cleared.
    pub is_static: bool,
    /// Non-suspending trace mode: armed with `EventThread`, each hit is snapshotted (including the
    /// old → new pair) into the trace ring buffer and the thread resumed (TRACE-2).
    pub trace: bool,
    /// Optional expression evaluated in the mutating frame and recorded on each trace hit.
    pub trace_expr: Option<String>,
    /// Remaining trace-hit budget (TRACE-3): each traced hit decrements it, and on reaching zero the
    /// watch disarms itself so a hot field can't flood the debuggee. `None` means unbounded.
    pub trace_budget: Option<u32>,
    /// How many caller frames each traced hit records above the mutating frame (TRACE-5).
    pub trace_frames: usize,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    /// Thread this watch is filtered to (`ThreadOnly`), if any — for the `list_stop_points` line (FILT-1).
    pub thread_filter: Option<u64>,
}

/// An active method-exit request (METH-1): a `METHOD_EXIT` / `METHOD_EXIT_WITH_RETURN_VALUE` request
/// reporting what a method returned, keyed by its `mexit_` id.
///
/// Tracked like every other stop point so `list_stop_points` shows it and `clear_stop_point` / `panic` /
/// `toggle_stop_point` handle it. A stop point this tool can create but not clear would be a SAFE-class
/// bug — and this is the kind least survivable if left armed, since a suspending method exit on a hot
/// method freezes the VM faster than anything else here.
#[derive(Debug, Clone)]
pub struct MethodExitRequestInfo {
    /// The `mexit_` id reported to the caller.
    pub id: String,
    /// The live JDWP request id, or `None` while disabled (BP-2).
    pub request_id: Option<i32>,
    pub enabled: bool,
    /// Dotted class pattern the caller gave, kept so a disabled request can be re-armed.
    pub class_pattern: String,
    /// Method name to report on, filtered on OUR side: JDWP has no method-name modifier, so the request
    /// fires for every method of a matching class and non-matching exits are dropped by the event pump.
    /// `None` means every method — only allowed in trace mode.
    pub method: Option<String>,
    /// Whether this was armed as `METHOD_EXIT_WITH_RETURN_VALUE` (kind 42). Needed to clear it, since
    /// JDWP keys requests by (eventKind, requestID); also says whether a hit can report a value at all.
    pub with_return_value: bool,
    /// Non-suspending trace mode — the default for this kind, and near-mandatory on a shared JVM.
    pub trace: bool,
    pub trace_expr: Option<String>,
    pub trace_budget: Option<u32>,
    /// Caller-frame depth for traced hits (TRACE-5).
    pub trace_frames: usize,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    pub thread_filter: Option<u64>,
}

/// A breakpoint waiting for its class to load. The `CLASS_PREPARE` request suspends the preparing
/// thread (`EventThread` policy) so the real breakpoint can be armed before any of the class's code
/// runs; the pump then resumes that one thread.
#[derive(Debug, Clone)]
pub struct PendingBreakpoint {
    /// The bp_ id reserved for this breakpoint (reported now, armed later).
    pub bp_id: String,
    /// The `CLASS_PREPARE` event-request id (cleared once armed).
    pub class_prepare_request_id: i32,
    /// Dotted class pattern (as the user gave it) — for messages.
    pub class_pattern: String,
    /// JNI signature ("Lpkg/Cls;") to match against the `ClassPrepare` event signature.
    pub signature: String,
    pub line: Option<i32>,
    pub method: Option<String>,
    pub hit_count: Option<i32>,
    pub thread_filter: Option<u64>,
    pub condition: Option<String>,
    /// Arm as a non-suspending trace/logpoint (`EventThread` suspend, snapshot, resume).
    pub trace: bool,
    /// Optional expression to evaluate and record on each trace hit.
    pub trace_expr: Option<String>,
    /// Trace-hit budget carried through to the real breakpoint once the class loads (TRACE-3).
    pub trace_budget: Option<u32>,
    /// Caller-frame depth carried through to the real breakpoint once the class loads (TRACE-5).
    pub trace_frames: usize,
}

#[derive(Debug, Clone)]
pub struct BreakpointInfo {
    /// The live JDWP request id, or `None` when the breakpoint is disabled — its definition is kept
    /// so it can be re-armed, but no request is set in the JVM (BP-1).
    pub request_id: Option<i32>,
    pub class_pattern: String,
    pub line: u32,
    pub method: Option<String>,
    /// Whether the breakpoint is currently armed in the JVM. A disabled breakpoint stays listed (so
    /// its `condition`/`trace_expr` aren't lost) but has no JDWP request and never fires (BP-1).
    pub enabled: bool,
    pub hit_count: u32,
    /// Optional server-side condition: on hit, evaluate it and auto-resume if it is not true.
    pub condition: Option<String>,
    /// Non-suspending trace/logpoint: on hit, snapshot into the ring buffer and resume the thread.
    pub trace: bool,
    /// Optional expression evaluated and recorded on each trace hit.
    pub trace_expr: Option<String>,
    /// Remaining trace-hit budget (TRACE-3); `None` means unbounded.
    pub trace_budget: Option<u32>,
    /// How many caller frames each traced hit records above the hit frame (TRACE-5). 0 restores the
    /// original one-frame snapshot.
    pub trace_frames: usize,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    /// Everything needed to re-arm this breakpoint at the same location after a `toggle_stop_point`
    /// disable (BP-1). Kept for every armed breakpoint so disable→enable round-trips exactly.
    pub arm: BreakpointArm,
}

/// The resolved JDWP location and modifiers for a breakpoint, kept so a disabled breakpoint can be
/// re-armed at exactly the same place with the same behaviour (BP-1).
#[derive(Debug, Clone)]
pub struct BreakpointArm {
    pub class_id: u64,
    pub method_id: u64,
    pub bytecode_index: u64,
    pub suspend_policy: jdwp_client::SuspendPolicy,
    pub hit_count: Option<i32>,
    pub thread_filter: Option<u64>,
}

#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<SessionId, Arc<Mutex<DebugSession>>>>>,
    current_session: Arc<Mutex<Option<SessionId>>>,
    /// Handed to every session it creates (EVT-2), so the event pump and the watchdog can push
    /// without a path back to the request handler.
    alerter: crate::protocol::Alerter,
}

impl SessionManager {
    pub fn new(alerter: crate::protocol::Alerter) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            current_session: Arc::new(Mutex::new(None)),
            alerter,
        }
    }

    pub async fn create_session(
        &self,
        connection: JdwpConnection,
        endpoint: String,
        read_only: bool,
        source_roots: Vec<std::path::PathBuf>,
    ) -> SessionId {
        let session_id = format!("session_{}", uuid::v4());
        let session = DebugSession {
            connection,
            endpoint,
            breakpoints: HashMap::new(),
            events: VecDeque::new(),
            event_seq: 0,
            events_dropped: 0,
            event_listener_task: None,
            last_thread: None,
            pending_step: None,
            suspended_since: None,
            suspended_cause: None,
            watchdog_task: None,
            last_watchdog_note: None,
            trace_disarms: std::collections::BTreeMap::new(),
            trace_disarms_dropped: 0,
            read_only,
            source_roots,
            pending_breakpoints: Vec::new(),
            exception_requests: HashMap::new(),
            watchpoints: HashMap::new(),
            method_exits: HashMap::new(),
            traces: VecDeque::new(),
            trace_seq: 0,
            stop_seq: 0,
            alerter: self.alerter.clone(),
        };

        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_id.clone(), Arc::new(Mutex::new(session)));
        drop(sessions); // release the map lock before taking the current-session lock

        // Set as current session
        let mut current = self.current_session.lock().await;
        *current = Some(session_id.clone());

        session_id
    }

    pub async fn get_current_session(&self) -> Option<Arc<Mutex<DebugSession>>> {
        let current = self.current_session.lock().await;
        if let Some(session_id) = current.as_ref() {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id).cloned()
        } else {
            None
        }
    }

    pub async fn get_session_by_id(&self, session_id: &str) -> Option<Arc<Mutex<DebugSession>>> {
        let sessions = self.sessions.lock().await;
        sessions.get(session_id).cloned()
    }

    pub async fn get_current_session_id(&self) -> Option<SessionId> {
        let current = self.current_session.lock().await;
        current.clone()
    }

    /// Every live session, id-sorted, with the current session's id.
    ///
    /// Sorted so repeated calls list them in the same order (a `HashMap` iteration order is arbitrary),
    /// and ids embed a timestamp, so this is oldest-first in practice.
    pub async fn list(&self) -> (Vec<(SessionId, Arc<Mutex<DebugSession>>)>, Option<SessionId>) {
        let sessions = self.sessions.lock().await;
        let mut rows: Vec<(SessionId, Arc<Mutex<DebugSession>>)> =
            sessions.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();
        drop(sessions); // release the map lock before taking the current-session lock
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        (rows, self.get_current_session_id().await)
    }

    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;

        // Abort the event listener task if it exists
        if let Some(session_arc) = sessions.get(session_id) {
            let mut session = session_arc.lock().await;
            if let Some(task) = session.event_listener_task.take() {
                task.abort();
            }
            if let Some(task) = session.watchdog_task.take() {
                task.abort();
            }
        }

        sessions.remove(session_id);
        drop(sessions); // release the map lock before taking the current-session lock

        // Clear current if it was this session
        let mut current = self.current_session.lock().await;
        if current.as_ref() == Some(&session_id.to_string()) {
            *current = None;
        }
    }
}

// Simple UUID generation for session IDs
mod uuid {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(1);

    pub fn v4() -> String {
        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        format!("{timestamp:x}{counter:x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // BP-3: ids are stable and unique per stop point, and independent of any JDWP request id — which is
    // what lets a disable → re-arm keep the id the caller is holding.
    #[test]
    fn stop_ids_are_sequential_and_prefixed() {
        let mut seq = 0u64;
        // Mirrors `next_stop_id` without needing a live connection to build a DebugSession.
        let mut next = |prefix: &str| {
            seq += 1;
            format!("{prefix}{seq}")
        };
        assert_eq!(next("bp_"), "bp_1");
        assert_eq!(next("exc_"), "exc_2");
        assert_eq!(next("watch_modify_"), "watch_modify_3");
        assert_eq!(next("bp_"), "bp_4", "ids must never be reused within a session");
    }

    /// Mirrors `DebugSession::note_trace_disarm` on bare state, so the bounding logic is testable
    /// without a live JDWP connection to build a whole session around.
    fn note_into(notes: &mut std::collections::BTreeMap<String, u32>, dropped: &mut u64, n: &str) {
        if let Some(c) = notes.get_mut(n) {
            *c += 1;
        } else if notes.len() < MAX_TRACE_DISARMS {
            notes.insert(n.to_string(), 1);
        } else {
            *dropped += 1;
        }
    }

    // SAFE-8: the disarm-note buffer must be bounded, collapse repeats, and count what it dropped.
    #[test]
    fn trace_disarm_notes_collapse_repeats_and_stay_bounded() {
        let mut notes: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        let mut dropped = 0u64;

        // The same stop point disarming repeatedly must not grow the buffer — this is the case BP-2/BP-3
        // made reachable, since a self-disarmed logpoint is now easy to re-arm.
        for _ in 0..500 {
            note_into(&mut notes, &mut dropped, "watch_3 stopped recording");
        }
        assert_eq!(notes.len(), 1, "repeats must collapse, not accumulate");
        assert_eq!(notes["watch_3 stopped recording"], 500, "the count is what carries the repetition");
        assert_eq!(dropped, 0);

        // Distinct notes are capped, and the overflow is counted rather than silently discarded.
        for i in 0..MAX_TRACE_DISARMS + 10 {
            note_into(&mut notes, &mut dropped, &format!("bp_{i} stopped recording"));
        }
        assert_eq!(notes.len(), MAX_TRACE_DISARMS, "distinct notes must be capped");
        assert!(dropped > 0, "overflow must be counted so a full buffer never reads as a quiet one");
    }

    // TRACE-7: a traced stop point that has captured nothing must be distinguishable from one that
    // captured for free. Every figure is absent, so the renderer has nothing to round down to 0.00ms.
    #[test]
    fn an_untouched_trace_cost_reports_no_figures_at_all() {
        let cost = TraceCost::default();
        assert_eq!(cost.captures, 0);
        assert!(cost.mean_capture().is_none(), "no captures means no mean, not a zero mean");
        assert!(cost.observed_rate().is_none());
        assert!(cost.capture_share().is_none());
    }

    // One capture prices a hit but establishes no interval, so there is a cost and no arrival rate. The
    // guard matters: dividing by a zero-width window would report an infinite rate for the quietest
    // possible trace.
    #[test]
    fn one_capture_gives_a_cost_but_no_arrival_rate() {
        let mut cost = TraceCost::default();
        let t0 = std::time::Instant::now();
        cost.record(t0, std::time::Duration::from_micros(800));
        assert_eq!(cost.mean_capture(), Some(std::time::Duration::from_micros(800)));
        assert!(cost.observed_rate().is_none(), "one capture spans no interval");
        assert!(cost.capture_share().is_none());
    }

    // Cost and arrival rate answer different questions and must not be conflated: 10 captures 100ms apart
    // is an arrival rate of 10/s, and each costing 1ms means 1% of the window went on capturing. That
    // product is the "is this hurting the instance?" number, and neither figure gives it alone.
    #[test]
    fn arrival_rate_and_capture_share_are_measured_over_the_observed_window() {
        let mut cost = TraceCost::default();
        let t0 = std::time::Instant::now();
        for i in 0..10u32 {
            cost.record(
                t0 + std::time::Duration::from_millis(u64::from(i) * 100),
                std::time::Duration::from_millis(1),
            );
        }
        assert_eq!(cost.captures, 10);
        assert_eq!(cost.mean_capture(), Some(std::time::Duration::from_millis(1)));

        // 10 captures span NINE 100ms intervals — 900ms, not a full second.
        let rate = cost.observed_rate().expect("ten captures span nine intervals");
        assert!((rate - 10.0).abs() < 0.01, "expected 10/s, got {rate}");

        let share = cost.capture_share().expect("both figures are present");
        assert!((share - 0.01).abs() < 0.0005, "expected ~1% of the window, got {share}");
    }

    // SAFE-4/SAFE-5: the two halves of "the VM is suspended" move together. Tracking them separately is
    // what let a manual pause record no cause at all, and a drain erase the offender.
    #[test]
    fn suspend_cause_distinguishes_a_stop_point_from_a_manual_pause() {
        assert_ne!(SuspendCause::ManualPause, SuspendCause::StopPoint(7));
        assert_eq!(SuspendCause::StopPoint(7), SuspendCause::StopPoint(7));
        assert_ne!(SuspendCause::StopPoint(7), SuspendCause::StopPoint(8));
    }
}
