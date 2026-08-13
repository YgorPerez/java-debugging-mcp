// Debug session management
//
// Manages JDWP connection state, breakpoints, and thread tracking

use crate::stop_point::StopPoint;
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
    /// Everything this session knows about its stop points, which is the half a test can build without a
    /// socket (CLEAN-6, #189).
    ///
    /// See [`SessionState`]. It is where an invariant spanning two of those collections lives, and the
    /// reason it is a separate type rather than more fields here is ADR-0050.
    pub state: SessionState,
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
    /// Active single-step request, as `(JDWP request id, the thread it was armed on)` — it must be
    /// cleared before the next resume, or it re-fires the instant threads run again.
    ///
    /// The **thread** joined the tuple with SAFE-11, and it is a pair rather than two fields for the same
    /// reason `suspended_since`/`suspended_cause` are set together: two fields that mean one thing drift,
    /// which is the bug SAFE-5 fixed. `debug.resume_thread` needs the thread, because releasing one thread
    /// that still has a step armed on it re-suspends it at the very next line — and JDWP's step events are
    /// `SuspendPolicy::All`, so a per-thread resume would freeze the WHOLE VM. That is a new way to leave
    /// the debuggee suspended, which is precisely what the resume-honesty matrix's `Freeze` list is for.
    pub pending_step: Option<(i32, u64)>,
    /// When the VM last suspended; cleared on resume. Drives the watchdog.
    pub suspended_since: Option<std::time::Instant>,
    /// **Why** the VM is suspended, recorded at suspension time and cleared on resume.
    ///
    /// The watchdog used to re-derive the offending stop point from the newest buffered event, which
    /// `get_last_event {drain:true}` could erase — so the polling caller `drain` exists for was exactly
    /// the one whose freeze never got disarmed (SAFE-5). One authoritative field instead of two sources
    /// of truth, and it also lets a manual `debug.pause` be told apart from a stop-point hit (SAFE-4).
    pub suspended_cause: Option<SuspendCause>,
    /// Threads this session is holding suspended **one at a time** (SAFE-11), keyed by thread id.
    ///
    /// Separate from [`suspended_since`](Self::suspended_since) on purpose, and the separation is the
    /// whole design. That field means *the VM is stopped* — every thread, nobody's request served — and
    /// `debug.continue` is what ends it. This one means *these N threads are stopped and the rest of the
    /// JVM is serving normally*, which is a different fact, ends a different way, and has a different
    /// blast radius. Collapsing them would make `debug.list_sessions` say `SUSPENDED` about a VM that is
    /// running fine, and would make `debug.pause`'s idempotency check refuse a pause because one worker
    /// was held.
    ///
    /// **It is bookkeeping, never the authority.** ADR-0003 rejected tracking our own suspend depth and
    /// resuming that many times, because the count drifts the moment anything outside this session
    /// suspends the same thread — another debugger, an IDE left attached, an `EventThread` event. So this
    /// records *what this session asked for*, and every reply about whether a thread is actually running
    /// still comes from `ThreadReference.SuspendCount`.
    ///
    /// A `BTreeMap` so listings and rescue notes name threads in a stable order rather than a hash order,
    /// matching [`redefinitions`](Self::redefinitions).
    pub thread_suspends: std::collections::BTreeMap<u64, ThreadSuspend>,
    pub watchdog_task: Option<JoinHandle<()>>,
    /// What the watchdog last did, if anything — surfaced in `list_stop_points` and `get_last_event`
    /// so a caller who was away learns the VM was auto-resumed and which stop point was disarmed (SAFE-2).
    pub last_watchdog_note: Option<String>,
    /// [`event_seq`](Self::event_seq) at the moment [`last_watchdog_note`](Self::last_watchdog_note)
    /// was written — the watermark that stops an old rescue from being replayed next to a new hit
    /// (SAFE-10).
    ///
    /// The note is not durable state, it is an account of **one** suspension ending, and without this it
    /// was rendered against every later event forever. That produced a `get_last_event` whose two lines
    /// were each correct and jointly false: `[suspended] true` for a genuinely live breakpoint hit, over a
    /// `[watchdog] auto-resumed the VM` about a suspension that had ended long before it — which reads as
    /// "the hit you are looking at is stale" about a hit that was fine, and cost a detour to re-verify.
    ///
    /// An event newer than the watermark means the suspension the note describes is not the one being
    /// rendered, so the note is that event's history rather than its state. SAFE-2's case is the other
    /// one and is untouched: a caller who walked away has no newer event, the watermark still matches,
    /// and they are told.
    pub last_watchdog_seq: Option<u64>,
    /// Rethrow chains in flight, keyed by `(request id, thread, exception object id)` (EXC-3).
    ///
    /// **The thread is in the key on purpose.** A rethrow unwinds on the thread that threw, so including
    /// it costs nothing and removes the one way this could misfire: JDWP object ids are reusable, so a
    /// later, unrelated exception handed the same id would otherwise be folded into a dead chain.
    pub rethrow_chains: HashMap<(i32, u64, u64), RethrowChain>,
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
    /// Directories `debug.reload_class` reads freshly compiled `.class` files from (SWAP-1), resolved
    /// once at attach from the tool argument or `JDWP_CLASS_ROOTS`.
    ///
    /// Separate from [`source_roots`](Self::source_roots) rather than shared with it, because they name
    /// different trees — `target/classes` against `src/main/java` — and a caller who set one has said
    /// nothing about the other. Same per-session reasoning otherwise: the build output belongs to the
    /// JVM you attached to.
    pub class_roots: Vec<std::path::PathBuf>,
    /// The `trace_expr` list every stop point in this session inherits when it names none (EVAL-14, #134).
    ///
    /// Per session for the same reason `source_roots` is: *step, look at the same six things, step again*
    /// is a property of the investigation, not of one stop point. Before this, a list belonged to the
    /// stop point that declared it (TRACE-11, #93), so arming a second one elsewhere meant restating the
    /// expressions and then joining two independently budgeted streams by hand.
    ///
    /// IT IS A DEFAULT, NOT AN OVERRIDE. A stop point that names its own `trace_expr` keeps it — the two
    /// are never merged, because a merge would silently push a caller's four-expression list past the cap
    /// and drop the end of it. Inheriting is reported in the arming reply rather than assumed, so a
    /// capture nobody asked for at this site is never a surprise.
    ///
    /// Evaluated only at a stop the caller already caused, inside a capture that was going to happen
    /// anyway. Never on events nobody asked to stop for: that would spend debuggee time on a JVM this
    /// project exists not to disturb, which is the same budget the 4-expression cap protects.
    pub trace_exprs: Vec<String>,
    /// The step filter every `debug.step_*` call in this session uses when it names none (STEP-2, #158).
    ///
    /// Per session for the reason `trace_exprs` is: the classes worth stepping over are a property of the
    /// application you attached to, not of one step, and stepping is the surface with the most calls per
    /// minute on it. STEP-1 (#94) shipped the per-call fields and they fixed the diving-into-WildFly
    /// problem; forgetting one on a single `step_into` still lands you forty frames inside a filter chain.
    ///
    /// `None` means the caller set nothing and the built-in default set applies, exactly as before this
    /// existed. `Some(vec![])` is a real answer and means *step into everything*.
    ///
    /// IT IS A DEFAULT, NOT AN OVERRIDE, AND THE PAIR IS ALL-OR-NOTHING. A step naming either of its own
    /// filter fields uses exactly what it named; neither of these is consulted then, including the half
    /// the call left out. See [`Self::step_only_classes`] and ADR-0040, whose three rules this follows.
    pub step_exclude_classes: Option<Vec<String>>,
    /// The `only_classes` half of this session's step-filter default (STEP-2, #158).
    ///
    /// Read as a pair with [`Self::step_exclude_classes`] — never merged with a call's own filter, and
    /// never applied on its own to a step that named the other field.
    pub step_only_classes: Option<Vec<String>>,
    /// Classes this session redefined and **cannot restore** (SWAP-2), keyed by class name.
    ///
    /// Its own bookkeeping because a redefinition is the only mutation here that outlives the thing that
    /// made it. Every other one — a field write, a forced return, an invoked method — is finished when the
    /// debuggee resumes; a redefined class keeps serving new bytecode after the resume, after the
    /// disconnect, and to everyone else on a shared instance, and only redeploying the artifact undoes it.
    ///
    /// This exists because of what it bought. SWAP-1's triage considered a third permission axis — a mode
    /// allowing `set_value` while still refusing to change the program — and rejected it on the grounds
    /// that reporting the residue is the honest answer to an unrepairable side effect, not a mode nobody
    /// remembers to set. That argument is only true if the reporting exists, which is this.
    ///
    /// A `BTreeMap` so a report lists classes in a stable order rather than a hash order, matching
    /// [`trace_disarms`](Self::trace_disarms).
    pub redefinitions: std::collections::BTreeMap<String, Redefinition>,
    /// Wildcard line-breakpoint families (FILT-3), keyed by their `bpset_` id.
    pub pattern_sets: HashMap<String, PatternStopSet>,
    /// The JVM this session STARTED, if any (LAUNCH-1) — `None` for an ordinary `debug.attach`, which is
    /// the difference between a debuggee whose lifetime is ours and one that belongs to somebody else.
    pub launched: Option<LaunchedJvm>,
    /// Halves of a monitor pair still waiting for their other half (DUMP-7, ADR-0035).
    ///
    /// **This exists because no monitor event carries an elapsed time.** `MONITOR_CONTENDED_ENTERED`
    /// reports that a thread got the lock and says nothing about how long it waited; `MONITOR_WAIT`
    /// carries the timeout the caller *asked* for, not what it got. "How long was it blocked" — the
    /// question a contention diagnosis is actually asking — is on neither half, so the only way to have it
    /// is to timestamp the opening half here and subtract on the closing one. Every reply that prints the
    /// result says the debugger measured it.
    ///
    /// **The key includes which pair.** `Object.wait()` releases its monitor and re-acquires it on wake,
    /// and that re-acquisition can itself be contended — so one thread can legitimately have a
    /// `Blocked`→`Acquired` and a `Wait`→`Waited` measurement outstanding on the *same* monitor at the
    /// same time. Keyed on (thread, monitor) alone they would overwrite each other and report one
    /// duration as the other.
    ///
    /// Bounded by [`MAX_MONITOR_PENDING`], because entries are removed by the *closing* half and there is
    /// no guarantee one ever arrives: a thread can die blocked, and arming only the opening half of a pair
    /// is a legitimate (cheaper) way to use this. Drops are counted rather than silent, like every other
    /// bounded buffer here.
    pub monitor_pending: HashMap<MonitorPairKey, std::time::Instant>,
    /// How many pending monitor halves were dropped because [`monitor_pending`](Self::monitor_pending)
    /// was full — reported, so a missing duration is explicable rather than mysterious.
    pub monitor_pending_dropped: u64,
    /// Ring buffer of trace/logpoint snapshots (see `TraceRecord`). Bounded by `MAX_TRACES`.
    pub traces: VecDeque<TraceRecord>,
    /// Monotonic sequence for trace records (survives ring-buffer eviction).
    pub trace_seq: u64,
    /// Push channel to the MCP client (EVT-2). Lives on the session because the two things that need
    /// it — the event pump and the watchdog — already hold a session and nothing else.
    ///
    /// It never replaces the `events` buffer above. A notification is best-effort and a client may
    /// never read one, so `debug.get_last_event` has to remain sufficient on its own.
    pub alerter: crate::protocol::Alerter,
}

/// A session's stop-point bookkeeping, and the half of a [`DebugSession`] that needs no socket.
///
/// **Why this is a type and not four more fields on `DebugSession`: ADR-0050.** The short version is that
/// `JdwpConnection` has exactly one constructor and it opens a `TcpStream` (ADR-0049), so anything reachable
/// only through a `DebugSession` is reachable only from a test that launches a JVM. Three assertions were
/// owed and unpayable for that reason alone, and [`Self::owns_live_request`] below is the one whose own doc
/// comment said so.
///
/// **What is in here is decided by the invariants, not by which fields are touched most.** These four are
/// exactly the state the methods below read or write together. `pattern_sets` is deliberately NOT among them
/// even though CLEAN-6 (#189) grouped it with the stop points: no invariant here spans it, and moving it
/// would have been churn wearing this commit's clothes.
#[derive(Debug)]
pub struct SessionState {
    /// Every stop point this session holds, of all five kinds, keyed by its **stop-point id** (ADR-0005).
    ///
    /// **One collection, not five** (CLEAN-4, #187). It was `breakpoints`, `exception_requests`,
    /// `watchpoints`, `method_exits` and `monitor_requests`, and the cost of that was not the five fields:
    /// it was that every question about a stop point — is it armed, is it **spent**, what glyph does it
    /// get, is its filter dead — had to be asked five times, so a fix to one of those rules could land on
    /// three kinds and miss two. See [`crate::stop_point::StopPoint`].
    ///
    /// A `HashMap`, so iteration order within a kind is unspecified — it always was. The order a *listing*
    /// groups kinds in is [`StopPointKind::LISTING_ORDER`](crate::stop_point::StopPointKind::LISTING_ORDER),
    /// which the renderer states rather than inheriting from this field's shape.
    pub stop_points: HashMap<String, StopPoint>,
    /// Breakpoints requested on classes not yet loaded. Each holds a `CLASS_PREPARE` request that
    /// fires when the class loads; the event pump then arms the real breakpoint via
    /// [`Self::resolve_pending`].
    pub pending_breakpoints: Vec<PendingBreakpoint>,
    /// JDWP request ids of **traced** stop points disarmed while events they generated were still in
    /// flight (TRACE-8, #72 — found while implementing EXC-3).
    ///
    /// **What goes wrong without it, and it is the worst failure this crate has.** `try_record_trace`
    /// recognises a traced hit by looking its request id up among the *enabled* requests. A budget disarm
    /// clears `request_id`, so a hit that the JVM had already generated stops being recognisable the
    /// instant the budget runs out — and the event falls through to the suspending path, which buffers it
    /// and leaves the thread suspended. Trace mode's entire promise is that it never freezes anything.
    ///
    /// It needs a rethrow to see, which is why it survived until #68: an exception stop that disarms on
    /// its budget mid-chain has three more throws of the same instance already coming. Measured on
    /// `RethrowProbe` — the probe stopped printing at the exact tick the budget ran out, and stayed
    /// stopped until a `debug.panic`.
    ///
    /// Bounded (`MAX_DISARMED_TRACED`), and nothing removes an entry — so membership alone must never be
    /// the whole test. [`was_traced_and_disarmed`](Self::was_traced_and_disarmed) carries the reason and
    /// the second clause; the short version is that a reused request id matching on membership would turn a
    /// **suspending** breakpoint into one that silently never suspends.
    pub disarmed_traced_requests: VecDeque<i32>,
    /// Monotonic counter behind caller-facing stop-point ids (`bp_`/`exc_`/`watch_`).
    ///
    /// Ids used to embed the JDWP request id, so re-arming a disabled stop point gave it a *new* id and
    /// silently broke any id the caller had stored — the thing that made `toggle_stop_point` awkward to
    /// script (BP-3). The request id is an internal detail now: still reported, never the identity.
    pub stop_seq: u64,
}

impl SessionState {
    /// A session's bookkeeping at the moment of attach — every collection empty, every counter at zero.
    ///
    /// **It takes no arguments, and that is the point of the type.** No socket, no runtime, no JVM: a test
    /// that wants a session holding two stop points builds one here and says so in three lines, where before
    /// it had to launch a debuggee (ADR-0050).
    #[must_use]
    pub fn new() -> Self {
        Self {
            stop_points: HashMap::new(),
            pending_breakpoints: Vec::new(),
            disarmed_traced_requests: VecDeque::new(),
            stop_seq: 0,
        }
    }

    /// Register an armed stop point under its own **stop-point id**.
    ///
    /// Takes the value rather than a key and a value, because the key **is** [`StopPoint::id`]: passing
    /// them separately is a way for the two to disagree, and every reply that names a stop point reads the
    /// field while `clear`, `toggle` and `get_traces` all address it by the key. One argument, one id.
    pub fn register_stop_point(&mut self, sp: StopPoint) {
        self.stop_points.insert(sp.id.clone(), sp);
    }

    /// Swap a **deferred** line breakpoint for the armed stop point it became, in one step. Answers
    /// whether a deferral of that id was actually there.
    ///
    /// **The two collections move together, and until this existed they did not.** The event pump did
    /// `pending_breakpoints.retain(…)` and [`Self::register_stop_point`] about ten lines apart, with a log
    /// line and — the part that matters — an `await` between them. For the width of that await the
    /// breakpoint was in **neither** collection: absent from `list_stop_points`, absent from the count, and
    /// absent from [`Self::owns_live_request`], which is the one that decides whether a hit already in
    /// flight gets surfaced or resumed and dropped.
    ///
    /// Nothing ever observed it, and the reason is worth stating because it is not a property of this
    /// state: the caller holds the session guard across the whole block, so no other task can look. That
    /// makes the invariant true by the caller's good behaviour rather than by construction — precisely the
    /// shape CLEAN-6 (#189) names, *an invariant that lives in whichever handler happens to update two
    /// fields together*. Here it cannot be got wrong, because there is no moment between the two writes.
    ///
    /// A caller that ignores the answer is registering a stop point for a deferral nobody recorded, which
    /// would mean two stop points with one id if the deferral were still live elsewhere.
    ///
    /// **It takes the stop point and no id.** A deferral and the stop point it becomes carry the *same*
    /// caller-facing id by definition — that is what makes `bp_4` still mean `bp_4` after the class loads
    /// (BP-3) — so passing both would invite a caller to pass two, and the pair that disagreed would remove
    /// one deferral while registering a different stop point.
    pub fn resolve_pending(&mut self, armed: StopPoint) -> bool {
        let before = self.pending_breakpoints.len();
        self.pending_breakpoints.retain(|p| p.bp_id != armed.id);
        let removed = before != self.pending_breakpoints.len();
        self.register_stop_point(armed);
        removed
    }

    /// Whether `req_id` belonged to a traced stop point that was disarmed with events in flight
    /// (TRACE-8, #72).
    ///
    /// **Membership in the list is not sufficient, and getting that wrong would be worse than the bug this
    /// fixes.** Nothing removes an id from the list, and JDWP request ids are allocated by the *debuggee* —
    /// `HotSpot` happens to hand them out monotonically, but the spec promises nothing, and this crate talks
    /// to whatever is on the port. If a reused id matched on membership alone, the hit it named would be
    /// resumed and dropped: a **suspending** breakpoint that silently never suspends, with no error
    /// anywhere and nothing in the reply to explain it. That is the same class of failure as the one being
    /// fixed, pointing the other way.
    ///
    /// So the id must also not currently belong to a live stop point. The caller has already established
    /// that it is not an *enabled traced* request (`find_traced_request` missed); this rules out its having
    /// been reused for an enabled **suspending** one, which is the case that matters. A stale entry then
    /// simply goes inert rather than needing to be purged.
    pub fn was_traced_and_disarmed(&self, req_id: i32) -> bool {
        self.disarmed_traced_requests.contains(&req_id) && !self.owns_live_request(req_id)
    }

    /// Whether any tracked stop point currently holds `req_id` as its live JDWP request.
    ///
    /// **All five kinds, which it did not used to be — and that is a behaviour change, not a refactor.**
    /// Written as four hand-repeated clauses it silently omitted the monitor kind: the sentence above said
    /// "any tracked stop point" and the code meant four of them. It fell out of CLEAN-4's one collection
    /// and landed in that commit, which is the wrong place for it — a change in behaviour behind an
    /// existing name belongs in a commit of its own, because `scripts/release-notes.py` builds the
    /// published changelog from commit **subjects** and a `refactor(…)` subject does not say this
    /// happened. This paragraph is the record it should have had.
    ///
    /// What changes: the guard's whole job is to stop [`Self::was_traced_and_disarmed`] matching on
    /// membership alone, because request ids are allocated by the *debuggee* and **recur**. With the
    /// monitor kind missing, a disarmed traced monitor request whose id the JVM had since reissued to a
    /// live one would answer `true` — and the hit it named would be resumed and dropped rather than
    /// surfaced. That is the same failure the list exists to prevent, pointing the other way, on one kind.
    ///
    /// **It had no test, and the reason was the seam rather than an oversight.** Reaching it needed a
    /// `DebugSession`, which owns a `JdwpConnection` and cannot be built without a socket (ADR-0049). That
    /// is what moving it onto [`SessionState`] fixed (CLEAN-6, #189, ADR-0050): the assertion is
    /// `a_live_stop_point_of_every_kind_owns_its_request`, it is driven off
    /// [`StopPointKind::LISTING_ORDER`](crate::stop_point::StopPointKind::LISTING_ORDER) so a sixth kind
    /// cannot dodge it, and `a_deferrals_class_prepare_counts_as_a_live_request` covers the second clause.
    /// Neither needs a JVM.
    fn owns_live_request(&self, req_id: i32) -> bool {
        self.stop_points.values().any(|sp| sp.owns_request(req_id))
            // A deferred breakpoint's CLASS_PREPARE is a live request too, and arming the real breakpoint
            // when it fires is not something to skip.
            || self.pending_breakpoints.iter().any(|p| p.class_prepare_request_id == req_id)
            // And an ARMED stop point keeps one for the rest of its life (BP-7, #115), which is how a copy
            // loaded by a redeploy's new classloader gets armed at all.
            || self
                .stop_points
                .values()
                .filter_map(crate::stop_point::StopPoint::line)
                .any(|l| l.rearm.watch().is_some_and(|w| w.request_id == req_id))
    }

    /// Remember that a traced stop point's JDWP request was just disarmed, so hits it had already
    /// generated are still resumed rather than surfaced as suspending events (TRACE-8, #72).
    pub fn note_disarmed_traced(&mut self, req_id: i32) {
        remember_bounded(&mut self.disarmed_traced_requests, req_id, MAX_DISARMED_TRACED);
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

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Max trace snapshots retained per session; oldest are evicted (documented cap for TRACE-1).
pub const MAX_TRACES: usize = 500;

/// Max distinct self-disarm notes retained per session (SAFE-8). Small on purpose: these are notices to
/// act on, not a log, and repeats of the same one are collapsed into a count rather than taking a slot.
pub const MAX_TRACE_DISARMS: usize = 32;

/// Max outstanding monitor pair-halves retained per session (DUMP-7, #96).
///
/// Sized for how many threads can *plausibly* be blocked or waiting at one instant on the app server this
/// tool exists for — Jetty's untuned default pool is 200 — rather than for the event rate, because an
/// entry lives only from one half of a pair to the other and is removed when it closes. What it has to
/// survive is the halves that never close: a thread that dies blocked, or a caller who armed the opening
/// half only.
///
/// **A full map evicts its oldest entry rather than refusing the new one**, which is the opposite of what
/// every other bound here does and is deliberate. Refusing would be self-defeating: the way this map fills
/// is with halves that will never close, so a refusal would stop measuring durations *permanently* the
/// first time 256 threads died blocked. Evicting the oldest keeps the mechanism alive and loses the entry
/// least likely to still be waiting for its partner. The eviction is counted
/// ([`monitor_pending_dropped`](DebugSession::monitor_pending_dropped)), so a duration that goes missing
/// this way is explicable.
pub const MAX_MONITOR_PENDING: usize = 256;

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
    /// FILT-7 ([#91](https://github.com/YgorPerez/java-debugging-mcp/issues/91)): set only when a
    /// conditional stop point's condition HELD but the escalation to a VM-wide suspend failed.
    ///
    /// A field on the record rather than a session flag, because it is a fact about **this hit** and the
    /// next hit may escalate cleanly. The state it names — matched, one thread held, application still
    /// running — is one no other field can express: the event's own suspend policy says a thread was
    /// suspended, and `suspended_since` says this session is holding something, and both are true while
    /// the VM is emphatically not stopped.
    pub escalation: Option<FailedEscalation>,
}

/// What a failed escalation left behind (FILT-7), as a reply has to state it.
///
/// Two fields rather than one sentence because the two answers have different audiences: `vm_running`
/// decides what `[suspended]` says and what the pushed notification claims, and `note` is the prose that
/// tells a caller what to do about it.
#[derive(Debug, Clone)]
pub struct FailedEscalation {
    /// Whether the application is still running, **as verified against the debuggee** rather than
    /// inferred from the failure — ADR-0003's rule applied to a suspend instead of a resume. `true` also
    /// covers "could not tell", because assuming the VM is running is the answer that makes a caller
    /// distrust the frame they are about to read, and distrusting a good frame costs less than trusting
    /// a moving one.
    pub vm_running: bool,
    /// The sentence `debug.get_last_event` prints, naming both halves of the state.
    pub note: String,
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

/// One thread `debug.suspend_thread` is holding (SAFE-11).
#[derive(Debug, Clone)]
pub struct ThreadSuspend {
    /// The thread's name, read once when it was suspended.
    ///
    /// Kept rather than re-read because the rescue path needs it most and can afford it least: the
    /// watchdog reporting "released 0x7f2c…" tells a caller nothing they can act on, and by the time it
    /// fires the thread may be gone, so asking then would answer with an error instead of a name.
    pub name: String,
    /// When this session **first** suspended it — the age the watchdog measures against
    /// `JDWP_WATCHDOG_SECS`, and the "held for" figure the listings show.
    ///
    /// Deliberately not refreshed by a second `debug.suspend_thread` on the same thread. The hazard the
    /// watchdog exists for is how long a worker has been off the pool, and that clock started at the
    /// first suspend; restarting it on every call would let a caller keep a thread frozen forever by
    /// suspending it repeatedly.
    pub since: std::time::Instant,
    /// How many `debug.suspend_thread` calls this session has made against this thread without a
    /// matching `debug.resume_thread`. Reported, never trusted — see
    /// [`thread_suspends`](DebugSession::thread_suspends).
    pub issued: u32,
}

/// One class this session redefined, and what is worth saying about it afterwards (SWAP-2).
#[derive(Debug, Clone)]
pub struct Redefinition {
    /// How many times this session redefined the class. An iterating caller reloads the same class
    /// repeatedly, and "17 times" is a different situation to report than "once".
    pub count: u32,
    /// When the most recent redefinition landed, for rendering "how long has this JVM been like this".
    pub at: std::time::Instant,
    /// Whether a frame in this class has been popped since the most recent redefinition.
    ///
    /// Worth tracking separately from the swap because the two failures are opposite. A redefinition
    /// nobody popped may not have taken effect at all in frames that were already running — the footgun
    /// `debug.reload_class` warns about — while one that *was* popped is fully live. The residue is real
    /// either way, so this changes what the report says, not whether it says anything.
    pub popped_since: bool,
}

/// The state of a session's event pump — the task that reads JDWP events off the connection, records
/// what a **traced** hit saw, and resumes the thread the event suspended (SESS-2, #195).
///
/// See [`DebugSession::event_pump`] for why there are three of these and not two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPump {
    /// Spawned and still reading. The only state in which this session can service an event, and
    /// therefore the only one in which arming a stop point on it is safe.
    Running,
    /// Spawned and has exited. The pump exits when the connection closes, so this is how a session
    /// learns its JVM is gone — at no cost, unlike a JDWP round trip, which could itself hang on the
    /// half-dead socket this is meant to diagnose.
    Ended,
    /// Never spawned: the session was registered and the attach that registered it did not finish
    /// building it. The socket is live and nobody is reading it. Reachable only as a failure, which is
    /// why it is a state and not a `bool` — it used to be indistinguishable from [`Self::Running`].
    Unstarted,
}

/// The methods a handler reaches a session through, rather than touching its fields.
///
/// **They share a verb vocabulary, and it is written down here because the next thing to touch this file
/// will add twenty more** (CLEAN-6, #189, whose whole subject is the ratio of direct field touches to
/// mediated calls). A convention nobody has stated is one that drifts on the commit that doubles it.
///
/// - **`note_…`** — record that something happened, so a later reply can report it. The session is not
///   acting; it is remembering. `note_redefinition`, `note_watchdog`, `note_disarmed_traced`,
///   `note_trace_disarm`, `note_pop`.
/// - **`mark_…`** — set a session state that something else will read as a fact about the VM.
///   `mark_suspended`, `mark_resumed`.
/// - **`open_…` / `close_…`** — one half of a pair's lifetime, where the other half completes it.
///   `open_monitor_pair`, `close_monitor_pair`.
/// - **`push_…`** — append to a **bounded** ring buffer, with eviction counted rather than silent.
///   `push_event`.
/// - **`register_…`** — add to a collection under an id the value itself carries. `register_stop_point`.
/// - Everything else is a query and reads as one: `was_traced_and_disarmed`, `watchdog_note_for`,
///   `classify_throw`, `next_stop_id`.
///
/// This is a naming convention and deliberately not an ADR: nothing here is surprising to a reader who
/// sees it, and nobody chose it against a real alternative. It is written where the next method will be
/// added, which is the only place it would have been read.
impl DebugSession {
    /// Whether the task draining this session's JDWP events is running (SESS-2, #195).
    ///
    /// **Three answers rather than a bool, because two of them are not the same fact.** A pump that has
    /// *ended* means the connection closed and the JVM is gone; a pump that was *never started* means this
    /// session was registered and then abandoned half-built, with a live socket and nobody reading it. The
    /// remedies differ, but the bug this exists for is that they used to give the same answer: liveness
    /// asked `event_listener_task.is_some_and(is_finished)`, so `None` — never spawned — read as *not
    /// dead*, which is exactly what a healthy session reads as.
    ///
    /// **What that costs is the whole of trace mode's promise.** Nothing else undoes a JDWP event's
    /// suspend policy: the pump is what snapshots a traced hit and resumes the thread it arrived on. With
    /// no pump, an armed trace stop point suspends at its first hit and stays suspended, while
    /// `debug.list_sessions` prints `running` and the arming reply reports success — which is what #195
    /// observed against a `WildFly` instance, where only restarting the JVM cleared it.
    #[must_use]
    pub fn event_pump(&self) -> EventPump {
        match self.event_listener_task.as_ref() {
            None => EventPump::Unstarted,
            Some(task) if task.is_finished() => EventPump::Ended,
            Some(_) => EventPump::Running,
        }
    }

    /// Record a successful redefinition of `class_name` (SWAP-2). Repeated swaps of one class collapse
    /// into a count, and any earlier pop stops counting — a pop applies to the bytecode that was live
    /// when it happened, not to whatever replaced it afterwards.
    pub fn note_redefinition(&mut self, class_name: &str) {
        let entry = self.redefinitions.entry(class_name.to_string()).or_insert_with(|| Redefinition {
            count: 0,
            at: std::time::Instant::now(),
            popped_since: false,
        });
        entry.count += 1;
        entry.at = std::time::Instant::now();
        entry.popped_since = false;
    }

    /// Record that a frame in `class_name` was popped, so a later report can distinguish a swap that is
    /// certainly live from one that may still be masked by frames that were already running. A pop in a
    /// class this session never redefined is not tracked — there is no residue to describe.
    pub fn note_pop(&mut self, class_name: &str) {
        if let Some(entry) = self.redefinitions.get_mut(class_name) {
            entry.popped_since = true;
        }
    }

    /// Push a reportable event, evicting the oldest if the buffer is full. Returns the assigned seq.
    ///
    /// `escalation` is FILT-7's failed-escalation note, and is `None` for every hit that did not have to
    /// escalate or escalated cleanly — see [`EventRecord::escalation`].
    pub fn push_event(&mut self, set: EventSet, escalation: Option<FailedEscalation>) -> u64 {
        self.event_seq += 1;
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
            self.events_dropped += 1;
        }
        self.events.push_back(EventRecord { seq: self.event_seq, set, escalation });
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
    pub const fn mark_resumed(&mut self) {
        self.suspended_since = None;
        self.suspended_cause = None;
    }

    /// Record what the watchdog just did, stamped with the event it happened after (SAFE-10).
    ///
    /// One method rather than two assignments at each of the watchdog's four outcomes, because a note
    /// written without its watermark is precisely the bug: it would be replayed against every later
    /// event, which is what #69 reported.
    pub fn note_watchdog(&mut self, note: String) {
        self.last_watchdog_note = Some(note);
        self.last_watchdog_seq = Some(self.event_seq);
    }

    /// Classify a traced exception hit as a first throw or a rethrow of an instance already captured,
    /// and advance that instance's chain (EXC-3, #68).
    ///
    /// **Why this exists.** An exception stop armed on an application type with `trace_max_hits: 30`
    /// captured 38 snapshots of which 30 were *one* instance walking `WildFly`'s EJB interceptor chain —
    /// `InterceptorContext.proceed` rethrowing at every layer. Two things went wrong and they compound:
    /// the stop point disarmed itself mid-request on a budget exhausted entirely on plumbing, and the one
    /// informative record — the original throw, with the application frame and the cause — was the 9th,
    /// reachable only by paging past the noise.
    ///
    /// **What is kept, and why not less.** Blanket dedupe by instance would be wrong: a rethrow at a
    /// *different site* can be the interesting one, and a wrapper that drops the cause is the exact
    /// failure this repo's swallowed-exception playbook exists for. So both ends survive — the first
    /// capture, and the latest sighting, which converges on the escape point as the chain unwinds — and
    /// only the middle is replaced by a count. The latest is a *rolling* record rather than a prediction:
    /// nothing here can know which rethrow is the last one, so each supersedes the previous, and whichever
    /// turns out to be final is the one left standing.
    ///
    /// Charging the budget is the caller's job, and [`ThrowKind::Rethrow`] means don't — that is the half
    /// that stops framework plumbing from spending a request's whole allowance.
    pub fn classify_throw(
        &mut self,
        req_id: i32,
        thread: u64,
        exception: Option<u64>,
        next_seq: u64,
    ) -> ThrowKind {
        let Some(exc) = exception else {
            return ThrowKind::First;
        };
        let key = (req_id, thread, exc);
        if let Some(chain) = self.rethrow_chains.get_mut(&key) {
            chain.collapsed = chain.collapsed.saturating_add(1);
            let supersedes = chain.rolling_seq.replace(next_seq);
            // The first rethrow is not a fold of anything yet — it becomes the rolling record, and only
            // the ones after it collapse into a count.
            return ThrowKind::Rethrow {
                fold: RethrowFold { collapsed: chain.collapsed - 1, first_seq: chain.first_seq },
                supersedes,
            };
        }
        // Evict the oldest chain rather than growing without bound. An exception whose chain is this stale
        // has been handled long ago, so the only thing lost is folding that will never be asked for.
        if self.rethrow_chains.len() >= MAX_RETHROW_CHAINS {
            if let Some(oldest) = self.rethrow_chains.iter().min_by_key(|(_, c)| c.first_seq).map(|(k, _)| *k)
            {
                self.rethrow_chains.remove(&oldest);
            }
        }
        self.rethrow_chains
            .insert(key, RethrowChain { first_seq: next_seq, rolling_seq: None, collapsed: 0 });
        ThrowKind::First
    }

    /// Record that the **opening** half of a monitor pair has arrived, so the closing half can measure
    /// the duration (DUMP-7, ADR-0035). Returns the eviction note when the map was full.
    ///
    /// An opening half arriving twice for the same key **overwrites** rather than being ignored. That is
    /// the honest reading: JDWP delivered a second "started blocking" without a matching "acquired", so
    /// either the first pair's close was never armed or the event was lost, and measuring the newer
    /// pair is right where measuring from a stale start would report a duration that includes work the
    /// thread was not blocked for.
    pub fn open_monitor_pair(&mut self, key: MonitorPairKey, at: std::time::Instant) {
        // Evict the OLDEST rather than refusing the new one — see `MAX_MONITOR_PENDING` for why this bound
        // behaves opposite to every other one here.
        if self.monitor_pending.len() >= MAX_MONITOR_PENDING && !self.monitor_pending.contains_key(&key) {
            if let Some(oldest) = self.monitor_pending.iter().min_by_key(|(_, t)| **t).map(|(k, _)| *k) {
                self.monitor_pending.remove(&oldest);
                self.monitor_pending_dropped = self.monitor_pending_dropped.saturating_add(1);
            }
        }
        self.monitor_pending.insert(key, at);
    }

    /// Close a monitor pair, returning how long it was open — the **debugger-measured** duration, since
    /// no monitor event carries one.
    ///
    /// `None` when this closing half has no matching opening half, which is a normal state rather than an
    /// error and has three innocent causes: the opening kind was never armed, the pair opened before
    /// this stop point was, or the entry was evicted. A reply that got `None` must say the duration is
    /// unavailable rather than print a zero, which would read as "it was not blocked at all".
    pub fn close_monitor_pair(
        &mut self,
        key: &MonitorPairKey,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        let opened = self.monitor_pending.remove(key)?;
        Some(now.saturating_duration_since(opened))
    }

    /// The watchdog note, but only when it is about the suspension a caller is *currently* looking at
    /// (SAFE-10) — `newest_seq` is the sequence of the newest event being rendered.
    ///
    /// `None` for a note that a later event has superseded. Also `None` when there is no event to render
    /// at all: with nothing on screen for the note to be misread as describing, `get_last_event` has its
    /// own answer for an empty buffer, and SAFE-2's caller-walked-away case always has the event that
    /// caused the suspension still in the buffer.
    pub fn watchdog_note_for(&self, newest_seq: Option<u64>) -> Option<&str> {
        let (note, at) = (self.last_watchdog_note.as_deref()?, self.last_watchdog_seq?);
        (newest_seq? <= at).then_some(note)
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
}

/// One value a snapshot captured: what it was called, how it rendered, and — for a reference — the
/// handle that reaches the same object again afterwards (TRACE-10, #85).
///
/// The id is carried **beside** the text rather than left inside it, because for most values it is not
/// in the text at all: a String renders as its contents, an array as its elements, a boxed primitive as
/// the number it holds, and an expanded object as a field block. Only the plain shallow render spells
/// an id out, and a snapshot that kept only the rendering was therefore a dead end for exactly the
/// values worth following up.
///
/// **The id is a weak reference and stays one.** Nothing pins it, so a handle may have *vanished* by
/// the time it is used — `CONTEXT.md` defines the term, and ADR-0022 records why pinning was rejected.
#[derive(Debug, Clone)]
pub struct TracedValue {
    pub name: String,
    /// The value as the snapshot rendered it — no `toString()` was invoked to produce it.
    pub rendered: String,
    /// The JDWP object id, for a non-null reference. `None` for a primitive and for `null`.
    pub object_id: Option<u64>,
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
    /// Each in-scope local/argument at the hit.
    pub args: Vec<TracedValue>,
    /// The enclosing method's captured locals, when the hit class is an **anonymous** inner class
    /// (TRACE-10, #85). Empty for every other class.
    ///
    /// `javac` compiles an anonymous class's captures to synthetic `val$…` fields plus a `this$0` back
    /// reference, and none of them are in `call()`'s local variable table — so a snapshot inside a
    /// fan-out worker showed one `this` and nothing about the request that queued it. These are read as
    /// **fields**, invoking nothing, so the capture stays side-effect free in trace mode.
    pub captured: Vec<TracedValue>,
    /// The calling chain above the hit frame, nearest caller first, each as `class.method:line`
    /// (TRACE-5). Empty when `trace_frames` was 0, or when the hit is already the outermost frame.
    ///
    /// **Locations only, deliberately.** The hit frame's locals are the payload; the callers are
    /// context, and reading every frame's variable table would multiply the per-hit cost on a logpoint
    /// that may fire hundreds of times. It also keeps the whole capture invocation-free, so caller
    /// chains work in a read-only session (SAFE-6) — unlike object expansion.
    pub callers: Vec<String>,
    /// `(expression, rendered result)` per trace expression, in the order the caller gave them
    /// (TRACE-11, #93). Empty when the logpoint had none.
    ///
    /// A `Vec` rather than a map because the ORDER is the caller's and is what makes the snapshot
    /// readable — and because two elements may legitimately be the same expression text, which a map
    /// would silently collapse. An element that failed to evaluate carries its own `<error: …>` in the
    /// value slot rather than being absent, so a snapshot always has one slot per expression asked for.
    pub expr: Vec<(String, String)>,
    /// What kind of stop point this came from, and anything specific to it: for an exception, the
    /// type and catch location; for a watchpoint, the field and its old → new pair. Empty for a
    /// plain line logpoint, whose location and args already say everything.
    ///
    /// Kept as ordered key/value pairs rather than a formatted string so the renderer, not the
    /// capture, decides how a trace line reads.
    pub detail: Vec<(String, String)>,
    /// Set when this snapshot is the *latest* sighting of an exception instance already captured
    /// earlier — the escaping end of a rethrow chain (EXC-3). `None` for every other snapshot.
    pub rethrow: Option<RethrowFold>,
}

/// A rethrow chain folded into one snapshot (EXC-3, #68).
#[derive(Debug, Clone, Copy)]
pub struct RethrowFold {
    /// How many rethrows of this instance were folded away to get here.
    pub collapsed: u32,
    /// `seq` of the first capture of this instance, so the original throw — the one with the
    /// application frame and the cause — can be found without paging.
    pub first_seq: u64,
}

/// What a traced exception hit is, with respect to chains already being tracked (EXC-3).
#[derive(Debug, Clone, Copy)]
pub enum ThrowKind {
    /// Not an exception hit at all, or an instance never seen before: record and charge it as usual.
    First,
    /// This instance was captured before, so this is a rethrow. `supersedes` is the seq of the rolling
    /// record to drop, if one is still in the buffer.
    Rethrow { fold: RethrowFold, supersedes: Option<u64> },
}

/// One exception instance's rethrow chain, while it unwinds (EXC-3).
#[derive(Debug, Clone, Copy)]
pub struct RethrowChain {
    /// `seq` of the first capture — never dropped, so the original throw survives.
    pub first_seq: u64,
    /// `seq` of the rolling "latest sighting" record, which each further rethrow replaces.
    pub rolling_seq: Option<u64>,
    pub collapsed: u32,
}

/// Push `req_id` onto a bounded, duplicate-free FIFO, evicting the oldest entry at `cap` (TRACE-8).
///
/// A free function rather than a method body so the bounding is testable without a live JDWP connection to
/// build a whole [`DebugSession`] around — the same reason `note_trace_disarm`'s logic is mirrored in this
/// module's tests, except that this one has no copy to drift from.
fn remember_bounded(queue: &mut std::collections::VecDeque<i32>, req_id: i32, cap: usize) {
    if queue.contains(&req_id) {
        return;
    }
    // A cap of 0 would otherwise push after failing to evict, growing the queue it is meant to bound.
    if cap == 0 {
        return;
    }
    if queue.len() >= cap {
        queue.pop_front();
    }
    queue.push_back(req_id);
}

/// How many disarmed traced request ids a session remembers (TRACE-8, #72).
///
/// Only in-flight events matter, and they arrive within microseconds of the disarm, so this is generous
/// rather than tuned — it exists to keep the queue from growing over a long session.
pub const MAX_DISARMED_TRACED: usize = 32;

/// How many rethrow chains a session tracks at once (EXC-3).
///
/// A chain lives only while one exception unwinds, so this is not a working-set size — it is a bound on
/// how far wrong the bookkeeping can go if entries are somehow never revisited. Evicting the oldest
/// chain loses only the folding of an exception that has long since been handled.
pub const MAX_RETHROW_CHAINS: usize = 64;

/// Which of the two monitor pairs a measurement belongs to (DUMP-7, #96).
///
/// Two rather than four, because the four event kinds are two *pairs*: a contended entry has a
/// beginning (`Blocked`) and an end (`Acquired`), and an `Object.wait()` has a beginning (`Wait`) and an
/// end (`Waited`). A duration is a property of the pair, not of either event in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MonitorPair {
    /// `MONITOR_CONTENDED_ENTER` → `MONITOR_CONTENDED_ENTERED`: how long a thread was **blocked** on a
    /// lock somebody else held. The contention figure a wedged app server is asked about.
    Contended,
    /// `MONITOR_WAIT` → `MONITOR_WAITED`: how long a thread sat in `Object.wait()`. A *voluntary* pause,
    /// so a long one is not by itself a fault — which is why it is not folded in with the above.
    Wait,
}

impl MonitorPair {
    /// Which pair an event kind belongs to, and whether it is the pair's opening half.
    #[must_use]
    pub const fn of(kind: jdwp_client::MonitorKind) -> (Self, bool) {
        match kind {
            jdwp_client::MonitorKind::Blocked => (Self::Contended, true),
            jdwp_client::MonitorKind::Acquired => (Self::Contended, false),
            jdwp_client::MonitorKind::Wait => (Self::Wait, true),
            jdwp_client::MonitorKind::Waited => (Self::Wait, false),
        }
    }

    /// How a reply names the duration this pair measures. Not "elapsed" for both: what the two pairs
    /// measure are different enough facts that one label for them would flatten the distinction the
    /// variants exist to keep.
    #[must_use]
    pub const fn duration_label(self) -> &'static str {
        match self {
            Self::Contended => "blocked_for",
            Self::Wait => "waited_for",
        }
    }
}

/// What identifies one outstanding monitor measurement: which thread, which monitor, which pair.
///
/// See [`DebugSession::monitor_pending`] for why the pair is in the key rather than left out as
/// redundant — a `wait` re-acquires its monitor on wake, so one thread can have both pairs open on one
/// object at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorPairKey {
    pub thread: u64,
    /// The monitor object. A **weak** reference like every object id here (ADR-0022) — it is only ever
    /// compared, never dereferenced, so a collected monitor costs an unmatched entry rather than an error.
    pub monitor: u64,
    pub pair: MonitorPair,
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
    /// The one object this stop point will be scoped to once it arms (`InstanceOnly`, FILT-9), if any.
    pub instance_filter: Option<u64>,
    pub thread_filter: Option<u64>,
    pub condition: Option<String>,
    /// Arm as a non-suspending trace/logpoint (`EventThread` suspend, snapshot, resume).
    pub trace: bool,
    /// Optional expression to evaluate and record on each trace hit.
    /// The trace expressions this stop point records, in the order given (TRACE-11, #93).
    /// Empty when it has none; one element is the pre-TRACE-11 case and renders identically.
    pub trace_expr: Vec<String>,
    /// Trace-hit budget carried through to the real breakpoint once the class loads (TRACE-3).
    pub trace_budget: Option<u32>,
    /// Caller-frame depth carried through to the real breakpoint once the class loads (TRACE-5).
    pub trace_frames: usize,
    /// Per-value length cap for this stop point's captures (TRACE-9), or `None` for the defaults (100
    /// for a local, 200 for the `trace_expr` result). Kept beside `trace_frames` and for the same
    /// reason: a disable and re-arm must not quietly hand back a narrower capture than the one armed.
    pub trace_max_length: Option<usize>,
}

/// A **family** of line breakpoints armed from one wildcard class pattern (FILT-3).
///
/// The thing a wildcard actually is: one caller intent (`break at the entry of handle on every
/// implementation of this interface`) that becomes N breakpoints, N JDWP requests and N line-table
/// lookups — plus an open-ended promise about classes that have not loaded yet.
///
/// **Why the members keep their own ids.** BP-3 says one id per stop point and the addressing tools lean
/// on it: `clear_stop_point`, `toggle_stop_point` and `list_stop_points`' per-request cost accounting
/// (ADR-0010) all key on a single `bp_…`. A wildcard does not change that — every armed location is an
/// ordinary line [`StopPoint`] under its own `bp_…` id and behaves exactly like one armed by name. This
/// record is what makes the *family* addressable as well, under a `bpset_…` id, and it exists because the
/// alternative is worse: a caller who armed 40 locations with one call and cannot un-arm them with one
/// call has been handed a mess, and a family that keeps arming new classes with no way to stop it would
/// be a stop point nobody can turn off — which is not something this server is allowed to build.
///
/// It is a distinct KIND of id, not a second way to address a breakpoint: `exc_`, `watch_` and `mexit_`
/// are already distinct kinds that `clear_stop_point` dispatches on by prefix, and `bpset_` joins them.
/// Clearing a member by its own `bp_…` still works and the family notices.
#[derive(Debug, Clone)]
pub struct PatternStopSet {
    /// The caller-facing `bpset_` id, stable for the family's whole life.
    pub id: String,
    /// The dotted wildcard pattern as the caller wrote it (`com.example.*`).
    pub class_pattern: String,
    /// The watch that arms classes loading from now on, and why it is not running when it isn't.
    ///
    /// The same primitive a deferred breakpoint uses, with one difference that matters: a deferred
    /// breakpoint clears its watch the moment its one class loads, and this one keeps it for as long as it
    /// can still use it. Every future matching class is new work, so the watch is the family's, not one
    /// breakpoint's, and disabling the family has to clear it or the family would keep growing while
    /// reporting itself silenced.
    pub watch: ClassLoadWatch,
    pub enabled: bool,
    /// `bp_` ids this family has armed, in arming order.
    pub members: Vec<String>,
    /// Classes armed AFTER the arming reply was written, by the event pump — a bounded sample of names.
    ///
    /// Reported by `list_stop_points` because it is the one part of a wildcard's cost that no reply could
    /// have stated: the caller was told "3 classes" and may now hold 9. Bounded like every other buffer
    /// here, with [`armed_later_total`](Self::armed_later_total) carrying the count the sample cannot.
    pub armed_later: Vec<String>,
    /// How many classes have been armed since the arming reply — the true count, never truncated.
    pub armed_later_total: usize,
    /// The location and behaviour every member is armed with — the family's definition, kept so a class
    /// loading in an hour is armed the same way the first one was.
    pub method: Option<String>,
    pub hit_count: Option<i32>,
    /// The object every member of this family is scoped to (`InstanceOnly`, FILT-9), if any.
    pub instance_filter: Option<u64>,
    pub thread_filter: Option<u64>,
    pub condition: Option<String>,
    pub trace: bool,
    /// The trace expressions this stop point records, in the order given (TRACE-11, #93).
    /// Empty when it has none; one element is the pre-TRACE-11 case and renders identically.
    pub trace_expr: Vec<String>,
    pub trace_budget: Option<u32>,
    pub trace_frames: usize,
    /// Per-value length cap for this stop point's captures (TRACE-9), or `None` for the defaults (100
    /// for a local, 200 for the `trace_expr` result). Kept beside `trace_frames` and for the same
    /// reason: a disable and re-arm must not quietly hand back a narrower capture than the one armed.
    pub trace_max_length: Option<usize>,
    /// Ceiling on live members, from `max_classes`.
    pub max_classes: usize,
    /// Matching classes NOT armed because the family was already full — reported, never silent.
    pub skipped_at_cap: usize,
    /// Matching classes that do not have the target method at all, which for a broad pattern is the
    /// expected majority rather than a failure.
    pub no_method: usize,
}

/// How many lines of a launched JVM's own output are kept.
pub const MAX_DEBUGGEE_OUTPUT: usize = 200;

/// A JVM **this server started**, owned by the session that started it (LAUNCH-1).
///
/// The thing that makes a launched JVM different from every other session here is that its lifetime is now
/// this process's problem. That was the argument against building this at all, and it is answered in three
/// places rather than assumed away:
///
/// - **Termination is decided at spawn time**, via `Command::kill_on_drop`. With the default
///   (`detach_on_disconnect: false`) dropping this record kills the JVM, so a session that goes away for any
///   reason — `debug.disconnect`, a dropped session map, a clean server exit — takes the process with it and
///   cannot leak a JVM with an open JDWP port. With `detach_on_disconnect: true` it is never killed, and the
///   caller has been told the lifetime is theirs.
/// - **A `SIGKILL`ed server still orphans it.** Putting the child in its own process group needs `pre_exec`,
///   which is `unsafe` and fails this workspace's lint gate (ADR-0007), so the honest answer is to say so:
///   the launch reply names the pid for exactly this case. Silence would have been the alternative, and
///   silence must never read as an answer.
/// - **Its output is captured, not inherited.** Inheriting is not an option: this server's stdout *is* the
///   MCP transport, and a debuggee printing to it would corrupt the protocol. So both streams are piped and
///   drained into this bounded buffer, which also stops a chatty program from filling a pipe and blocking on
///   a debugger that never reads it.
#[derive(Debug)]
pub struct LaunchedJvm {
    /// OS process id — reported so a caller can kill it themselves if this server dies badly.
    pub pid: Option<u32>,
    /// The full command line, for reporting: "which JDK, which classpath" is the question a
    /// version-dependent bug turns on.
    pub command: String,
    /// The child handle. Killing on drop is configured at spawn from `detach_on_disconnect`.
    pub child: tokio::process::Child,
    /// The debuggee's own stdout/stderr, most recent last, bounded by [`MAX_DEBUGGEE_OUTPUT`].
    ///
    /// A `std::sync::Mutex` rather than the async one used elsewhere here, and for a reason that shows up in
    /// the reply: `debug.list_sessions` renders a session line synchronously, and a launched JVM that has
    /// DIED needs its last words on that line — the alternative is a session reported `DEAD` with the
    /// explanation sitting in a buffer nothing can reach. Nothing holds this lock across an await.
    pub output: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    /// Leave the JVM running when the session ends.
    pub detach_on_disconnect: bool,
}

impl LaunchedJvm {
    /// The last `n` captured output lines, oldest first. Empty if the buffer is poisoned — a debuggee's
    /// stdout is never worth propagating a panic for.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let Ok(buf) = self.output.lock() else {
            return Vec::new();
        };
        buf.iter().skip(buf.len().saturating_sub(n)).cloned().collect()
    }
}

/// A wildcard family's **class-load watch**, and why it is not running when it isn't (FILT-5).
///
/// This was an `Option<i32>` — a request id or nothing — until "nothing" turned out to mean three
/// different things that a caller has to be able to tell apart, and that the code has to act on
/// differently. The one that made it an enum is [`Parked`](Self::Parked): a family at `max_classes` used to
/// keep its watch, so every class the JVM loaded still cost an event, a suspension of the loading thread
/// and a resume — to be told there is no room. `max_classes` bounded what a wildcard may *arm* and left
/// what it *costs* unbounded, which made a full family read as "done paying" when it meant "paying the same
/// and buying nothing".
///
/// The three not-watching states are not interchangeable in the listing either: a parked watch comes back on
/// its own the moment a slot frees, a disabled one comes back when the caller re-arms the family, and a
/// failed one never comes back at all. Rendering them the same way would tell a caller their family will grow
/// when it won't, or that it is broken when it is merely full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassLoadWatch {
    /// Registered under this JDWP request id: matching classes that load from now on are armed.
    Watching(i32),
    /// Deliberately not registered, because the family is FULL at `max_classes` and a watch could only
    /// cost. The family keeps its definition and starts watching again the moment a member is cleared.
    Parked,
    /// Cleared because the family is disabled (BP-1). Re-arming re-registers it — unless it is still full.
    Disabled,
    /// The JVM refused to register it, so this family will never grow. Its members are unaffected, which
    /// is why this does not fail the arming call.
    Failed,
}

impl ClassLoadWatch {
    /// The live JDWP request id, when there is one to clear.
    pub const fn request_id(&self) -> Option<i32> {
        match self {
            Self::Watching(req) => Some(*req),
            Self::Parked | Self::Disabled | Self::Failed => None,
        }
    }

    /// Is this family currently being told about classes as they load?
    pub const fn is_watching(&self) -> bool {
        matches!(self, Self::Watching(_))
    }
}

/// How many newly-armed class names one family keeps for reporting.
const MAX_ARMED_LATER_SAMPLE: usize = 25;

impl PatternStopSet {
    /// Is there room for another member?
    pub fn has_room(&self) -> bool {
        self.members.len() < self.max_classes
    }

    /// Record a class armed after the arming reply: the count always, the name while there is room.
    pub fn note_armed_later(&mut self, class: &str) {
        self.armed_later_total += 1;
        if self.armed_later.len() < MAX_ARMED_LATER_SAMPLE {
            self.armed_later.push(class.to_string());
        }
    }
}

#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<SessionId, Arc<Mutex<DebugSession>>>>>,
    current_session: Arc<Mutex<Option<SessionId>>>,
    /// Handed to every session it creates (EVT-2), so the event pump and the watchdog can push
    /// without a path back to the request handler.
    alerter: crate::protocol::Alerter,
}

/// Everything a new session takes from the call that opened it (STEP-2, #158).
///
/// A struct rather than six positional parameters: `create_session` was already at the
/// `clippy::too_many_arguments` line before the step filter added two, and the six are one idea anyway —
/// what `debug.attach` and `debug.launch` set once instead of on every later call. It is the same
/// grouping, and for the same reason, that `SessionDefaults` in `handlers.rs` already applies one layer
/// up; this is its landing point.
pub struct SessionSeed {
    pub read_only: bool,
    pub source_roots: Vec<std::path::PathBuf>,
    pub class_roots: Vec<std::path::PathBuf>,
    pub trace_exprs: Vec<String>,
    pub step_exclude_classes: Option<Vec<String>>,
    pub step_only_classes: Option<Vec<String>>,
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
        seed: SessionSeed,
    ) -> SessionId {
        let SessionSeed {
            read_only,
            source_roots,
            class_roots,
            trace_exprs,
            step_exclude_classes,
            step_only_classes,
        } = seed;
        let session_id = format!("session_{}", uuid::v4());
        let session = DebugSession {
            state: SessionState::new(),
            connection,
            endpoint,
            events: VecDeque::new(),
            event_seq: 0,
            events_dropped: 0,
            event_listener_task: None,
            last_thread: None,
            pending_step: None,
            suspended_since: None,
            suspended_cause: None,
            thread_suspends: std::collections::BTreeMap::new(),
            watchdog_task: None,
            last_watchdog_note: None,
            last_watchdog_seq: None,
            rethrow_chains: HashMap::new(),
            trace_disarms: std::collections::BTreeMap::new(),
            trace_disarms_dropped: 0,
            read_only,
            source_roots,
            class_roots,
            trace_exprs,
            step_exclude_classes,
            step_only_classes,
            redefinitions: std::collections::BTreeMap::new(),
            pattern_sets: HashMap::new(),
            launched: None,
            monitor_pending: HashMap::new(),
            monitor_pending_dropped: 0,
            traces: VecDeque::new(),
            trace_seq: 0,
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

    /// Make an already-registered session the current one (SESS-1, #157).
    ///
    /// The third writer of the current-session cell, beside registration and removal — and the first that
    /// is not a side effect of the set of sessions changing. Returns the id it displaced, or `None` if
    /// there was no current session.
    ///
    /// **Validation is the caller's**, deliberately: this is reached only after the handler has looked the
    /// id up and found the session live, and duplicating the lookup here would mean two answers to
    /// "does this session exist" that can disagree. What this owns is the write.
    ///
    /// Sends no JDWP packet and touches no debuggee. Selecting a session is a fact about this server's
    /// bookkeeping, and a tool that reached the JVM to change which one is current would be able to fail
    /// for reasons that have nothing to do with the question.
    pub async fn make_current(&self, session_id: &str) -> Option<SessionId> {
        let mut current = self.current_session.lock().await;
        current.replace(session_id.to_string())
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
    //
    // **This used to mirror `next_stop_id` instead of calling it** — a closure in the test body
    // reimplemented the two lines and asserted on the reimplementation, because a `DebugSession` could not
    // be built without a socket. It could not fail on any change to the real function, which is the
    // **vacuous** verdict `CONTEXT.md` defines. It calls the real one now (CLEAN-6, #189).
    #[test]
    fn stop_ids_are_sequential_and_prefixed() {
        let mut state = SessionState::new();
        assert_eq!(state.next_stop_id("bp_"), "bp_1");
        assert_eq!(state.next_stop_id("exc_"), "exc_2");
        assert_eq!(state.next_stop_id("watch_modify_"), "watch_modify_3");
        assert_eq!(state.next_stop_id("bp_"), "bp_4", "ids must never be reused within a session");
    }

    /// A deferral, in the state a test needs it and nothing more.
    ///
    /// `signature` and `class_prepare_request_id` are the two that carry meaning here; everything else is
    /// the quietest value that compiles, the same move `stop_point::build::armed` makes.
    fn pending(bp_id: &str, class_prepare_request_id: i32) -> PendingBreakpoint {
        PendingBreakpoint {
            bp_id: bp_id.to_string(),
            class_prepare_request_id,
            class_pattern: "com.example.Orders".to_string(),
            signature: "Lcom/example/Orders;".to_string(),
            line: Some(42),
            method: None,
            hit_count: None,
            instance_filter: None,
            thread_filter: None,
            condition: None,
            trace: false,
            trace_expr: Vec::new(),
            trace_budget: None,
            trace_frames: 0,
            trace_max_length: None,
        }
    }

    /// CLEAN-6 (#189): the assertion [`SessionState::owns_live_request`]'s own doc comment asked for.
    ///
    /// That paragraph said the test belongs here and named the seam as the reason it did not exist —
    /// reaching the function needed a `DebugSession`, which owns a `JdwpConnection` and cannot be built
    /// without a socket (ADR-0049). This is the assertion arriving, and it needs no JVM.
    ///
    /// **Every kind, which is the behaviour the doc comment records as a change.** Written as four
    /// hand-repeated clauses it silently omitted the monitor kind: a disarmed traced monitor request whose
    /// id the JVM had since reissued to a live one answered `true`, and the hit it named would be resumed
    /// and dropped rather than surfaced. Driven off `LISTING_ORDER` so a sixth kind cannot be added without
    /// this covering it.
    #[test]
    fn a_live_stop_point_of_every_kind_owns_its_request() {
        for kind in crate::stop_point::StopPointKind::LISTING_ORDER {
            let mut state = SessionState::new();
            state.register_stop_point(crate::stop_point::build::armed("sp_1", kind));
            assert!(
                state.owns_live_request(7),
                "{kind:?}: an armed stop point must own its live request. If it does not, \
                 `was_traced_and_disarmed` matches on list membership alone and a hit already in flight is \
                 resumed and dropped — a suspending stop point that silently never suspends"
            );
            assert!(!state.owns_live_request(8), "{kind:?}: and it must not claim a request it never held");
        }
    }

    /// A deferred breakpoint's `CLASS_PREPARE` is a live request too — the second clause of
    /// [`SessionState::owns_live_request`], and the one that keeps a deferral from being armed twice.
    #[test]
    fn a_deferrals_class_prepare_counts_as_a_live_request() {
        let mut state = SessionState::new();
        state.pending_breakpoints.push(pending("bp_1", 99));
        assert!(state.owns_live_request(99), "the CLASS_PREPARE that will arm the real breakpoint is live");
        assert!(!state.owns_live_request(7), "and nothing else is");
    }

    /// TRACE-8 (#72): **membership alone must never be the whole test**, which is the rule
    /// [`SessionState::was_traced_and_disarmed`] exists to enforce and had no assertion for.
    ///
    /// Request ids are allocated by the *debuggee* and recur. If a reused id matched on membership, the hit
    /// it named would be resumed and dropped — the same failure the list prevents, pointing the other way.
    #[test]
    fn a_disarmed_traced_request_stops_matching_once_its_id_is_live_again() {
        let mut state = SessionState::new();
        state.note_disarmed_traced(7);
        assert!(
            state.was_traced_and_disarmed(7),
            "a disarmed traced request is recognised while its id is free"
        );

        // The debuggee reissues 7 to a live stop point — `build::armed` arms on exactly that id.
        state.register_stop_point(crate::stop_point::build::armed(
            "bp_1",
            crate::stop_point::StopPointKind::Line,
        ));
        assert!(
            !state.was_traced_and_disarmed(7),
            "once 7 belongs to a live stop point it must NOT read as disarmed-and-traced: the hit would be \
             resumed and dropped instead of surfaced"
        );
        assert!(
            state.disarmed_traced_requests.contains(&7),
            "and the entry goes inert rather than being purged"
        );
    }

    /// CLEAN-6 (#189): resolving a deferral is **one step**, so there is no moment when the breakpoint is
    /// in neither collection.
    ///
    /// The event pump used to `retain` the pending list and `register_stop_point` about ten lines apart with
    /// an `await` between them. This asserts the property that replaced it: after one call the id is gone
    /// from the deferrals and present among the stop points, and the count of things this session holds
    /// never dipped.
    #[test]
    fn a_resolved_deferral_moves_between_the_two_collections_in_one_step() {
        let mut state = SessionState::new();
        state.pending_breakpoints.push(pending("bp_1", 99));
        assert_eq!(state.pending_breakpoints.len() + state.stop_points.len(), 1);

        let armed = crate::stop_point::build::armed("bp_1", crate::stop_point::StopPointKind::Line);
        assert!(state.resolve_pending(armed), "the deferral was there, so it must report having removed it");

        assert!(state.pending_breakpoints.is_empty(), "the deferral is gone");
        assert!(state.stop_points.contains_key("bp_1"), "and the armed stop point kept the caller's id");
        assert_eq!(
            state.pending_breakpoints.len() + state.stop_points.len(),
            1,
            "one thing before and one thing after — a caller counting stop points never sees zero"
        );
    }

    /// Resolving something never deferred answers `false`, which is how the event pump notices it is about
    /// to have two records claiming one id.
    #[test]
    fn resolving_a_deferral_that_was_never_pending_says_so() {
        let mut state = SessionState::new();
        let armed = crate::stop_point::build::armed("bp_1", crate::stop_point::StopPointKind::Line);
        assert!(!state.resolve_pending(armed), "nothing was pending, and the caller is told");
        assert!(state.stop_points.contains_key("bp_1"), "the stop point is still registered");
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

    // TRACE-8 (#72): the disarmed-traced list is what stops a budget disarm from freezing the VM with the
    // hits it had already generated, so its bounding is load-bearing — an unbounded one would grow for the
    // life of a session, and a broken eviction would forget the id that is about to be needed.
    #[test]
    fn disarmed_traced_ids_are_deduplicated_and_bounded() {
        let mut q = std::collections::VecDeque::new();

        // The same request disarmed repeatedly is one entry: `disarm_request` is reached from the budget
        // path, the watchdog and a manual clear, and a re-armed stop point can disarm again and again.
        for _ in 0..500 {
            remember_bounded(&mut q, 7, MAX_DISARMED_TRACED);
        }
        assert_eq!(q.len(), 1, "repeats must not accumulate");

        // Oldest out, newest in — the newest is the one whose hits are still in flight.
        for i in 0..i32::try_from(MAX_DISARMED_TRACED).unwrap_or(i32::MAX) + 5 {
            remember_bounded(&mut q, 1000 + i, MAX_DISARMED_TRACED);
        }
        assert_eq!(q.len(), MAX_DISARMED_TRACED, "the list must stay bounded");
        assert!(!q.contains(&7), "the oldest entry must be the one evicted");
        let newest = 1000 + i32::try_from(MAX_DISARMED_TRACED).unwrap_or(i32::MAX) + 4;
        assert!(q.contains(&newest), "the newest disarm is the one most likely to have a hit in flight");

        // A zero cap must not grow the queue it is meant to bound.
        let mut zero = std::collections::VecDeque::new();
        remember_bounded(&mut zero, 1, 0);
        assert!(zero.is_empty(), "cap 0 must store nothing rather than push after not evicting");
    }

    // TRACE-7: a traced stop point that has captured nothing must be distinguishable from one that
    // captured for free. Every figure is absent, so the renderer has nothing to round down to 0.00ms.
    #[test]
    fn an_untouched_trace_cost_reports_no_figures_at_all() {
        let cost = crate::stop_point::TraceCost::default();
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
        let mut cost = crate::stop_point::TraceCost::default();
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
        let mut cost = crate::stop_point::TraceCost::default();
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
