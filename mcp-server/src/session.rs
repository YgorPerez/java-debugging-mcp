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
    /// Everything reportable this session has seen, and the watchdog note scoped to it (CLEAN-6, #189).
    ///
    /// See [`EventRing`]. The rescue note lives there rather than here because the watermark that scopes
    /// it is the ring's own sequence — the invariant that put the two in one type.
    pub events: EventRing,
    pub event_listener_task: Option<JoinHandle<()>>,
    /// Thread of the most recent suspension event — used to default `thread_id`.
    pub last_thread: Option<u64>,
    /// Active single-step request, as `(JDWP request id, the thread it was armed on)` — it must be
    /// cleared before the next resume, or it re-fires the instant threads run again.
    ///
    /// The **thread** joined the tuple with SAFE-11, and it is a pair rather than two fields for the same
    /// reason [`VmSuspend`] is one value rather than two `Option`s: two fields that mean one thing drift,
    /// which is the bug SAFE-5 fixed. `debug.resume_thread` needs the thread, because releasing one thread
    /// that still has a step armed on it re-suspends it at the very next line — and JDWP's step events are
    /// `SuspendPolicy::All`, so a per-thread resume would freeze the WHOLE VM. That is a new way to leave
    /// the debuggee suspended, which is precisely what the resume-honesty matrix's `Freeze` list is for.
    pub pending_step: Option<(i32, u64)>,
    /// Everything this session is holding suspended: the whole VM, individual threads, or both
    /// (CLEAN-6, #189). See [`Suspensions`] — the two are separate facts on purpose, and the type is
    /// where that separation, and ADR-0003's rule that none of it is the authority, are written down.
    pub suspensions: Suspensions,
    pub watchdog_task: Option<JoinHandle<()>>,
    /// Every **snapshot** this session has taken, and the accounting that makes a missing one explicable
    /// (CLEAN-6, #189). See [`TraceBuffer`].
    pub traces: TraceBuffer,
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
    /// Classes this session redefined and **cannot restore** (SWAP-2, CLEAN-6 #189).
    ///
    /// See [`Redefinitions`]. It is a type rather than a map because a swap has to un-do an earlier pop,
    /// and that rule had nowhere to be asserted while it lived here.
    pub redefinitions: Redefinitions,
    /// Wildcard line-breakpoint families (FILT-3), keyed by their `bpset_` id.
    pub pattern_sets: HashMap<String, PatternStopSet>,
    /// The JVM this session STARTED, if any (LAUNCH-1) — `None` for an ordinary `debug.attach`, which is
    /// the difference between a debuggee whose lifetime is ours and one that belongs to somebody else.
    pub launched: Option<LaunchedJvm>,
    /// The debugger's own stopwatch across monitor pairs (DUMP-7, ADR-0035, CLEAN-6 #189).
    ///
    /// See [`MonitorClock`]. It is a type rather than two fields because an unclosed half must never reach
    /// a later request, and that rule was three lines in three handlers.
    pub monitor_clock: MonitorClock,
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
/// least likely to still be waiting for its partner. The eviction is counted in [`MonitorClock::dropped`]
/// — and read by nothing, which that field's own doc records rather than repeating the claim that a
/// duration lost this way is explicable to a caller.
pub const MAX_MONITOR_PENDING: usize = 256;

/// Max reportable events retained per session; oldest are evicted, counted in [`EventRing::dropped`].
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
    /// suspended, and `suspensions.vm` says this session is holding something, and both are true while
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

/// A session's reportable events, and the accounting that makes a missing one explicable (CLEAN-6, #189).
///
/// **The fourth cluster ADR-0050 describes, and chosen the same way: by invariant, not by touch count.**
/// [`Self::push`] moves the ring, the sequence and the drop count together, and that much #189 already
/// named as one cluster. The watchdog note is here because of what stamps it.
///
/// **SAFE-10's watermark is why the note belongs to the ring rather than to the session.** A note is an
/// account of **one** suspension ending, and what scopes it to that suspension is
/// [`watchdog_seq`](Self::watchdog_seq) — the ring's sequence at the moment the note was written. Written
/// without the watermark it was rendered against every later event forever, which is #69: a
/// `get_last_event` whose two lines were each correct and jointly false, `[suspended] true` for a
/// genuinely live breakpoint hit over a `[watchdog] auto-resumed the VM` about a suspension that had ended
/// long before it. So the note cannot be *stamped* without reading the counter — split them across two
/// types and [`Self::note_watchdog`] needs the ring passed in, which is the invariant back in a caller.
/// #189 grouped the note with the bounded note collections; the counter it depends on decides otherwise,
/// and that is the one place this cluster departs from the issue's list.
///
/// **`get_last_event` reads it through the watermark. `list_stop_points` does not, and there is a
/// pre-existing hole under that.** SAFE-10 scoped the one tool that renders the note beside an event; the
/// listing prints it raw at the top, and *nothing* ever clears it — not [`Self::clear`], not a resume, not
/// a fresh suspension. So one rescue at minute five captions every later listing for the life of the
/// session, including after the stop point it names has been re-armed and hit again, which is #69's shape
/// in the other tool that prints the note. It predates this type and is not made worse by it; it is
/// written down here because moving the note next to its watermark would otherwise read as a claim that
/// every reader consults it, and one does not.
///
/// **Nothing resets the sequence, including [`Self::clear`].** It is the identity a pushed notification
/// has already handed to a client and the number the watermark compares against, so a reused one would
/// name two events. [`dropped`](Self::dropped) survives a clear for the reason [`TraceBuffer::filed`]
/// does: a session that fell behind must not read afterwards as one that never did.
///
/// **It is constructible with no socket**, which is the point of the type (ADR-0049, ADR-0050) — a test
/// that wants a ring holding a hit and a stale rescue note builds one here in three lines.
#[derive(Debug)]
pub struct EventRing {
    /// Reportable events, oldest first. Bounded by [`MAX_EVENTS`].
    ///
    /// A single `Option` slot here used to mean a second hit erased the first with no trace — the worst
    /// kind of gap in a debugging tool, because the answer you read looks complete. Traces got a bounded
    /// buffer from the start; events got the same treatment.
    pub held: VecDeque<EventRecord>,
    /// Monotonic sequence for event records. Survives eviction **and** a [`Self::clear`].
    pub seq: u64,
    /// How many events the ring has evicted — reported, so a caller knows it fell behind.
    pub dropped: u64,
    /// What the watchdog last did, if anything — surfaced in `list_stop_points` and `get_last_event` so a
    /// caller who was away learns the VM was auto-resumed and which stop point was disarmed (SAFE-2).
    pub watchdog_note: Option<String>,
    /// [`seq`](Self::seq) at the moment [`watchdog_note`](Self::watchdog_note) was written — the watermark
    /// that stops an old rescue from being replayed next to a new hit (SAFE-10).
    ///
    /// An event newer than the watermark means the suspension the note describes is not the one being
    /// rendered, so the note is that event's history rather than its state. SAFE-2's case is the other one
    /// and is untouched: a caller who walked away has no newer event, the watermark still matches, and
    /// they are told. The type's own doc has what it cost to be absent.
    pub watchdog_seq: Option<u64>,
}

/// A slice of the ring as a reply has to state it: what is being shown, and what is not.
///
/// One value rather than three returns because the three figures are one answer — see [`EventRing::tail`],
/// whose doc is where the reason they cannot be computed apart lives.
#[derive(Debug)]
pub struct EventTail {
    /// The events to render, oldest first — so the **newest is last**, matching `get_traces`, and a bare
    /// call prints exactly the latest event as it always did.
    pub shown: Vec<EventRecord>,
    /// How many older events are still buffered but were not shown, so a caller knows a larger `limit`
    /// has something to read.
    pub unshown: usize,
    /// How many the ring has evicted since the session opened — [`EventRing::dropped`], carried here so a
    /// reply that has to explain a gap reads one value.
    pub dropped: u64,
}

impl EventRing {
    /// An empty ring: nothing seen, nothing dropped, no rescue to report.
    #[must_use]
    pub const fn new() -> Self {
        Self { held: VecDeque::new(), seq: 0, dropped: 0, watchdog_note: None, watchdog_seq: None }
    }

    /// Push a reportable event, evicting the oldest if the ring is full. Returns the assigned seq.
    ///
    /// `escalation` is FILT-7's failed-escalation note, and is `None` for every hit that did not have to
    /// escalate or escalated cleanly — see [`EventRecord::escalation`].
    ///
    /// **The seq is assigned before the eviction, and the record carries it.** An evicted event does not
    /// give its number back: the sequence counts what arrived, and [`dropped`](Self::dropped) counts what
    /// eviction took — which is what lets a reply explain a gap instead of printing a shorter list. The two
    /// are not `seq - held.len()`, because [`Self::clear`] moves that difference without dropping anything.
    pub fn push(&mut self, set: EventSet, escalation: Option<FailedEscalation>) -> u64 {
        self.seq += 1;
        if self.held.len() >= MAX_EVENTS {
            self.held.pop_front();
            self.dropped += 1;
        }
        self.held.push_back(EventRecord { seq: self.seq, set, escalation });
        self.seq
    }

    /// The newest `limit` events, and what the caller is not being shown.
    ///
    /// **The figures have to agree, which is why this is one method rather than four lines in the
    /// handler.** [`EventTail::unshown`] is precisely what the tail skipped, so it has to come from the
    /// same clamped count the slice did — computed a second time beside a differently-clamped `limit` it
    /// would tell a caller to pass a larger one for events that were already on screen.
    ///
    /// A `limit` of zero still yields one event, because a bare `debug.get_last_event` means *the latest*
    /// and always has; a `limit` past the end shows everything with nothing unshown.
    #[must_use]
    pub fn tail(&self, limit: usize) -> EventTail {
        let total = self.held.len();
        let take = limit.max(1).min(total);
        let unshown = total - take;
        EventTail { shown: self.held.iter().skip(unshown).cloned().collect(), unshown, dropped: self.dropped }
    }

    /// Discard the events held, as `debug.get_last_event {drain: true}` does.
    ///
    /// **It keeps the sequence, the drop count and the watchdog note**, each for its own reason: the
    /// sequence is an identity a notification has already handed out, the drop count is the only evidence
    /// the ring ever fell behind, and the note is what `list_stop_points` prints to a caller who walked
    /// away (SAFE-2). A drain says the caller has read the events, not that the rescue never happened.
    ///
    /// A method rather than `held.clear()` at the call site so that rule has somewhere to be asserted —
    /// with the field cleared directly, "a drain must not reset the counter" is true only for as long as
    /// nobody adds the obvious second line.
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// The watchdog note, but only when it is about the suspension a caller is *currently* looking at
    /// (SAFE-10) — `newest_seq` is the sequence of the newest event being rendered.
    ///
    /// `None` for a note that a later event has superseded. Also `None` when there is no event to render
    /// at all: with nothing on screen for the note to be misread as describing, `get_last_event` has its
    /// own answer for an empty buffer, and SAFE-2's caller-walked-away case always has the event that
    /// caused the suspension still in the buffer.
    #[must_use]
    pub fn watchdog_note_for(&self, newest_seq: Option<u64>) -> Option<&str> {
        let (note, at) = (self.watchdog_note.as_deref()?, self.watchdog_seq?);
        (newest_seq? <= at).then_some(note)
    }

    /// Record what the watchdog just did, stamped with the event it happened after (SAFE-10).
    ///
    /// One method rather than two assignments at each of the watchdog's four outcomes, because a note
    /// written without its watermark is precisely the bug: it would be replayed against every later
    /// event, which is what #69 reported.
    pub fn note_watchdog(&mut self, note: String) {
        self.watchdog_note = Some(note);
        self.watchdog_seq = Some(self.seq);
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
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
    /// [`Suspensions::threads`].
    pub issued: u32,
}

/// A VM-wide suspension: when it started, and why. **One value, because the pair has no legal half.**
///
/// It was `suspended_since: Option<Instant>` and `suspended_cause: Option<SuspendCause>`, which admits
/// four states for a fact that has two. Two of the four are the bug SAFE-5 fixed — a manual pause that
/// recorded a timestamp and no cause, so the watchdog resumed the VM and could not say what had frozen it —
/// and `mark_suspended`/`mark_resumed` were added to keep them in step. That is mediation, and it worked;
/// this is the same invariant held by construction instead, which is the difference ADR-0050 argues for.
///
/// What it removes at the call sites is the guards against the states that can no longer occur:
/// `suspended_since.map_or(0, …)` next to a cause the code had already matched as `Some`, and the
/// watchdog's `None => "(cause unrecorded)"` arm, which existed only because a suspension could have a
/// clock and no reason.
#[derive(Debug, Clone, Copy)]
pub struct VmSuspend {
    /// When the VM suspended. Drives the watchdog, and the "how long ago" every reply about an
    /// already-suspended VM prints.
    pub since: std::time::Instant,
    /// **Why** the VM is suspended, recorded at suspension time rather than re-derived.
    ///
    /// The watchdog used to work the offending stop point out from the newest buffered event, which
    /// `get_last_event {drain:true}` could erase — so the polling caller `drain` exists for was exactly
    /// the one whose freeze never got disarmed (SAFE-5). One authoritative field instead of two sources
    /// of truth, and it also lets a manual `debug.pause` be told apart from a stop-point hit (SAFE-4).
    pub cause: SuspendCause,
}

/// A session's suspension bookkeeping, and the third cluster ADR-0050 describes (CLEAN-6, #189).
///
/// **The two fields are two different facts, and keeping them apart is the whole design.** [`Self::vm`]
/// means *the VM is stopped* — every thread, nobody's request served — and `debug.continue` is what ends
/// it. [`Self::threads`] means *these N threads are stopped and the rest of the JVM is serving normally*,
/// which ends a different way and has a different blast radius. Collapsing them would make
/// `debug.list_sessions` say `SUSPENDED` about a VM that is running fine, and would make `debug.pause`'s
/// idempotency check refuse a pause because one worker was held. They are in one type because the
/// **watchdog reads both on every tick** and rescues them on separate arms, which is the invariant that
/// spans them: a session can be in either state, both, or neither, and each has to be able to fire alone.
///
/// **None of it is the authority, and that is ADR-0003.** Tracking our own suspend depth and resuming
/// that many times was the rejected alternative, because the count drifts the moment anything outside this
/// session suspends the same thread — another debugger, an IDE left attached, an `EventThread` event. So
/// this records *what this session asked for*, every reply about whether a thread is actually running
/// comes from `ThreadReference.SuspendCount`, and [`Self::forget_thread`] is how a claim the JVM has
/// contradicted stops being made. Until this type existed, that rule had nowhere to be asserted: the
/// bookkeeping lived on a [`DebugSession`], which owns a [`JdwpConnection`] and cannot be built without a
/// socket (ADR-0049), so every one of the rules below was enforced by a handler and tested by nothing.
///
/// **It is constructible with no socket**, which is the point of the type — a test that wants a session
/// holding two threads since three minutes ago builds one here in four lines.
#[derive(Debug)]
pub struct Suspensions {
    /// The VM-wide suspension, or `None` when the debuggee is running.
    pub vm: Option<VmSuspend>,
    /// Threads this session is holding suspended **one at a time** (SAFE-11), keyed by thread id.
    ///
    /// A `BTreeMap` so listings and rescue notes name threads in a stable order rather than a hash order,
    /// matching [`DebugSession::redefinitions`] — and [`Self::forget_all_threads`] returns that order to
    /// the disconnect reply.
    pub threads: std::collections::BTreeMap<u64, ThreadSuspend>,
}

impl Suspensions {
    /// A session's suspension state at the moment of attach: the VM is running and no thread is held.
    #[must_use]
    pub const fn new() -> Self {
        Self { vm: None, threads: std::collections::BTreeMap::new() }
    }

    /// Record that the VM is now suspended, and why.
    ///
    /// **It overwrites, and the callers do not all agree that is safe.** `debug.pause` and
    /// `debug.thread_dump` refuse to suspend a VM that is already suspended, because a second suspend
    /// builds a counted depth one resume cannot undo *and* would replace a `StopPoint` cause with
    /// `ManualPause`, losing the SAFE-2 disarm — so the refusal lives with the caller that knows why it
    /// is refusing, and what this must not do is silently keep the older clock, which would make the
    /// watchdog measure a suspension against the wrong start.
    ///
    /// **The event pump is the caller that does not check, and there is a known hole under it.** It
    /// records a cause on every suspending event, deliberately including one whose escalation to a
    /// VM-wide suspend FAILED — that hit's thread is still held, and this is the only record of it. But a
    /// failed escalation leaves the VM *running*, so a second hit arrives and overwrites both halves: the
    /// watchdog then disarms the newer request and the older one is never disarmed, which is the SAFE-2
    /// loss the paragraph above describes. It predates this type and is not made worse by it; it is
    /// written down here because the honest reading of "it overwrites" is that one caller has not decided
    /// anything about it.
    pub fn mark_suspended(&mut self, cause: SuspendCause) {
        self.vm = Some(VmSuspend { since: std::time::Instant::now(), cause });
    }

    /// Record that the VM is running again. Every resume path calls this, so nothing is left stale.
    ///
    /// **It says nothing about [`Self::threads`]**, deliberately. `debug.continue` clears the VM's suspend
    /// depth, which is a different count from a per-thread suspend, and a thread held by
    /// `debug.suspend_thread` is still held afterwards — which is exactly what `verify_thread_suspends`
    /// tells the caller instead of letting them assume otherwise.
    pub const fn mark_resumed(&mut self) {
        self.vm = None;
    }

    /// Record that this session has suspended thread `tid`, and return how many suspends of it this
    /// session is now claiming — the `ours` figure the reply prints.
    ///
    /// **A second suspend of the same thread does not restart its clock**, and that is the invariant this
    /// method exists to hold rather than to describe. [`ThreadSuspend::since`] is the age the watchdog
    /// measures against `JDWP_WATCHDOG_SECS`; the hazard it exists for is how long a worker has been off
    /// the pool, and that clock started at the *first* suspend. Refreshing it would let a caller keep a
    /// thread frozen forever by suspending it repeatedly — a rescue that never fires, produced by a line
    /// that looks like an update to a timestamp.
    pub fn hold_thread(&mut self, tid: u64, name: String, at: std::time::Instant) -> u32 {
        let entry = self.threads.entry(tid).or_insert_with(|| ThreadSuspend { name, since: at, issued: 0 });
        entry.issued = entry.issued.saturating_add(1);
        entry.issued
    }

    /// Drop **one** of this session's claims on `tid`, as `debug.resume_thread` does — one call, one
    /// decrement (ADR-0003). Returns the record as it stood *before* the decrement, which is what the
    /// reply names the thread and its age from, or `None` when this session was not holding it.
    ///
    /// **The record goes when our own count reaches zero, even if the JVM still reports depth.** Whatever
    /// is left then is not ours, and keeping the entry would put this session's name on somebody else's
    /// suspension — including in the watchdog's rescue list, which would resume a thread we never held.
    pub fn release_thread(&mut self, tid: u64) -> Option<ThreadSuspend> {
        let rec = self.threads.get_mut(&tid)?;
        let before = rec.clone();
        rec.issued = rec.issued.saturating_sub(1);
        if rec.issued == 0 {
            self.threads.remove(&tid);
        }
        Some(before)
    }

    /// Stop claiming `tid` **entirely**, whatever this session's own count says. Answers whether there
    /// was a claim to drop.
    ///
    /// This is ADR-0003's rule in its bookkeeping half: the JVM is the authority, so a thread it says is
    /// running — or one that has ended, or one a `debug.panic` resumed to a count of zero — is not ours to
    /// claim any more, and how many times *we* asked is beside the point. Distinct from
    /// [`Self::release_thread`] for exactly that reason: one is a caller giving a thread back, the other
    /// is a claim expiring against evidence, and decrementing here would leave a record for a thread
    /// nothing is holding.
    pub fn forget_thread(&mut self, tid: u64) -> bool {
        self.threads.remove(&tid).is_some()
    }

    /// Stop claiming every thread, returning their names in the map's stable order — `debug.disconnect`'s
    /// case, where `VirtualMachine.Dispose` has already resumed them all.
    ///
    /// The names come back because the reply has to say which threads went: the claim is being dropped
    /// because it is no longer true, and a caller who left a worker suspended is the one person who needs
    /// to be told it was released.
    pub fn forget_all_threads(&mut self) -> Vec<String> {
        let names = self.threads.values().map(|r| r.name.clone()).collect();
        self.threads.clear();
        names
    }

    /// The threads this session has held for at least `secs` — the watchdog's second arm (SAFE-11).
    ///
    /// **Only the overdue ones.** A thread suspended ten seconds ago is a caller at work, not a leak, and
    /// sweeping it up with one held for three minutes would make the tool unusable for its purpose.
    ///
    /// Measured from [`ThreadSuspend::since`], which is why this and [`Self::hold_thread`] cannot be
    /// reasoned about apart: a thread suspended repeatedly still becomes overdue, because the first
    /// suspend set the clock and no later one moves it.
    ///
    /// `now` is a parameter for the same reason [`MonitorClock::open`]'s `at` is — a caller
    /// wanting two figures about one moment must be able to pass the same moment to both, and a test must
    /// be able to name a moment five minutes on without depending on how long the machine has been up.
    /// `Instant` has no portable origin, so *backdating* is the operation with no safe form here.
    #[must_use]
    pub fn overdue_threads(&self, secs: u64, now: std::time::Instant) -> Vec<u64> {
        self.threads
            .iter()
            .filter(|(_, r)| now.saturating_duration_since(r.since).as_secs() >= secs)
            .map(|(t, _)| *t)
            .collect()
    }

    /// How long the oldest of `tids` has been held — the "held up to" figure a rescue note prints.
    ///
    /// Zero when none of them is held, which a caller reaches only by asking about threads that were
    /// released underneath it.
    ///
    /// Takes `now` so a rescue measures this against the very instant it selected the threads with
    /// [`Self::overdue_threads`], rather than a few microseconds later.
    #[must_use]
    pub fn longest_held(&self, tids: &[u64], now: std::time::Instant) -> std::time::Duration {
        self.threads
            .iter()
            .filter(|(t, _)| tids.contains(t))
            .map(|(_, r)| now.saturating_duration_since(r.since))
            .max()
            .unwrap_or_default()
    }
}

impl Default for Suspensions {
    fn default() -> Self {
        Self::new()
    }
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

/// Classes this session redefined and **cannot restore** (SWAP-2), and the sixth and last cluster
/// ADR-0050 describes (CLEAN-6, #189).
///
/// **Its own bookkeeping because a redefinition is the only mutation here that outlives the thing that
/// made it.** Every other one — a field write, a forced return, an invoked method — is finished when the
/// debuggee resumes; a redefined class keeps serving new bytecode after the resume, after the disconnect,
/// and to everyone else on a shared instance, and only redeploying the artifact undoes it.
///
/// This exists because of what it bought. SWAP-1's triage considered a third permission axis — a mode
/// allowing `set_value` while still refusing to change the program — and rejected it on the grounds that
/// reporting the residue is the honest answer to an unrepairable side effect, not a mode nobody remembers
/// to set. That argument is only true if the reporting exists, which is this.
///
/// **The invariant that chose this cluster: a pop belongs to the bytecode that was live when it
/// happened.** [`Self::note_pop`] and [`Self::note_swap`] write the same record from opposite directions,
/// and a swap has to *un-do* an earlier pop — the frames that were running under the old code are gone,
/// so whether the newest swap reached the frames still running is once again unknown. Reported either way;
/// what it changes is which of the two sentences the residue report prints, and that sentence is what tells
/// the next person what to check. It lived in `DebugSession` and so could be asserted by nothing.
///
/// **It is constructible with no socket**, which is the point of the type (ADR-0050).
#[derive(Debug)]
pub struct Redefinitions {
    /// Keyed by class name, in a `BTreeMap` so a report lists classes in a stable order rather than a hash
    /// order — matching [`TraceBuffer::disarms`], and asserted by the renderer's own tests.
    pub held: std::collections::BTreeMap<String, Redefinition>,
}

impl Redefinitions {
    /// A session that has redefined nothing, which is nearly every session.
    #[must_use]
    pub const fn new() -> Self {
        Self { held: std::collections::BTreeMap::new() }
    }

    /// Record a successful redefinition of `class_name`. Repeated swaps of one class collapse into a
    /// count, because an iterating caller reloads the same class over and over and "17 times" is a
    /// different situation to report than "once".
    ///
    /// **It moves the clock and clears any earlier pop, and both are the same rule.** `at` is what the
    /// report renders "how long has this JVM been like this" from, so it has to name the *newest* swap;
    /// and a pop recorded before that swap was about bytecode this one has just replaced, so continuing to
    /// count it would report "a frame was popped since, so the new code is live" about code no frame has
    /// been popped under.
    ///
    /// `at` is a parameter for the reason [`MonitorClock::open`]'s is: a test must be able to name a moment
    /// five minutes on without depending on how long the machine has been up, and `Instant` has no portable
    /// origin, so *backdating* is the operation with no safe form here.
    pub fn note_swap(&mut self, class_name: &str, at: std::time::Instant) {
        let entry = self.held.entry(class_name.to_string()).or_insert_with(|| Redefinition {
            count: 0,
            at,
            popped_since: false,
        });
        entry.count += 1;
        entry.at = at;
        entry.popped_since = false;
    }

    /// Record that a frame in `class_name` was popped, so a later report can distinguish a swap that is
    /// certainly live from one that may still be masked by frames that were already running.
    ///
    /// **A pop in a class this session never redefined is not tracked, and does not create a record.**
    /// There is no residue to describe: popping a frame is not itself a thing a caller has to be warned
    /// they cannot undo, and an entry made here would put a class into the residue report that this
    /// session never changed.
    pub fn note_pop(&mut self, class_name: &str) {
        if let Some(entry) = self.held.get_mut(class_name) {
            entry.popped_since = true;
        }
    }
}

impl Default for Redefinitions {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything a traced session has recorded, and everything it can no longer show you (CLEAN-6, #189).
///
/// **The second cluster ADR-0050 describes, and chosen the same way: by invariant, not by touch count.**
/// These five fields are exactly the state [`Self::push`] and [`Self::clear`] read or write together. A
/// snapshot's `seq` comes from the counter, the counter is what tells a reader how many records the ring
/// has dropped, a **fold** evicts the sighting it supersedes *from the ring*, and a disarm note explains a
/// gap the ring cannot — so no two of them can be moved apart without leaving the invariant in a handler.
///
/// **The invariant that was living in a handler.** `file_trace_record` in `handlers.rs` bumped `trace_seq`,
/// stamped it onto the record, classified the throw, evicted the superseded sighting and rang the buffer —
/// five writes across four fields, in one function, reachable only from the event pump and therefore
/// (ADR-0049) only from a test that launches a JVM. `session.rs` could not assert any of it. The ordering
/// is not incidental either: the seq has to be assigned *before* [`Self::classify_throw`], because the
/// chain records it as the sighting a later rethrow will supersede.
///
/// **Two counters that are not the same counter.** [`filed`](Self::filed) counts every record ever taken;
/// `held.len()` is what survives. `debug.get_traces` and the investigation report both print the
/// difference, and that difference is the only thing telling a reader that the earliest hits of a long
/// trace are gone — which is why the counter is not just the ring's length plus a drop count. A **fold**
/// makes them disagree a second way, and legitimately: a superseded sighting is removed from the ring
/// without being lost, because the fold that replaced it carries `first_seq`.
#[derive(Debug)]
pub struct TraceBuffer {
    /// Snapshots, oldest first. Bounded by [`MAX_TRACES`].
    pub held: VecDeque<TraceRecord>,
    /// How many snapshots have ever been filed here — **not** how many are held.
    ///
    /// Monotonic and never reset by eviction, so it survives the ring: it is what a **snapshot**'s `#seq`
    /// is, and `filed - held.len()` is what a reader cannot see any more.
    pub filed: u64,
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
    /// Repeats are **collapsed** rather than appended, and the map is capped ([`MAX_TRACE_DISARMS`]).
    /// It was an unbounded `Vec`, which only looked harmless while an auto-disarm also deleted the stop
    /// point: BP-2/BP-3 made re-arming easy, so one budgeted logpoint can now disarm over and over. Every
    /// other buffer here is bounded, and "`watch_3` disarmed itself 12 times" beats identical lines
    /// anyway (SAFE-8).
    pub disarms: std::collections::BTreeMap<String, u32>,
    /// How many distinct disarm notes were dropped because the map was full — reported, like
    /// [`EventRing::dropped`], so a full buffer never reads as a quiet one.
    pub disarms_dropped: u64,
}

impl TraceBuffer {
    /// An empty buffer — no snapshots, no chains, no notes, nothing filed.
    ///
    /// **It takes no arguments, which is the point of the type** (ADR-0050): a test that wants a buffer
    /// holding three folded rethrows builds one here, where before it had to launch a debuggee and
    /// persuade it to throw the same instance four times.
    #[must_use]
    pub fn new() -> Self {
        Self {
            held: VecDeque::new(),
            filed: 0,
            rethrow_chains: HashMap::new(),
            disarms: std::collections::BTreeMap::new(),
            disarms_dropped: 0,
        }
    }

    /// File one **snapshot**: stamp its `seq`, fold it if it is a rethrow, and ring the buffer.
    ///
    /// Returns the [`ThrowKind`], which is what decides whether the caller charges the **trace budget** —
    /// a collapsed rethrow is not a new finding and does not spend one.
    ///
    /// **Every write the record needs, in the one order that works.** The seq is assigned before the
    /// classification because [`Self::classify_throw`] stores it as the sighting a later rethrow of the
    /// same instance will supersede; classifying first would fold each record into the one before it. The
    /// eviction of the superseded record happens before the ring's own eviction, so a fold cannot push an
    /// unrelated snapshot out to make room for a record that is about to replace one anyway.
    ///
    /// The record arrives with `seq` and `rethrow` unset and leaves with both decided here, so no caller
    /// can file one carrying a seq it chose itself.
    pub fn push(
        &mut self,
        mut rec: TraceRecord,
        req_id: i32,
        thread: u64,
        exception: Option<u64>,
    ) -> ThrowKind {
        self.filed += 1;
        rec.seq = self.filed;
        let kind = self.classify_throw(req_id, thread, exception, rec.seq);
        if let ThrowKind::Rethrow { fold, supersedes } = kind {
            rec.rethrow = Some(fold);
            // The previous latest-sighting of this instance is what this record replaces, so it goes.
            // Absent when the buffer already evicted it, which needs no repair — the fold's own
            // `first_seq` still points at the original throw.
            if let Some(old) = supersedes {
                self.held.retain(|r| r.seq != old);
            }
        }
        if self.held.len() >= MAX_TRACES {
            self.held.pop_front();
        }
        self.held.push_back(rec);
        kind
    }

    /// Record that a traced stop point disarmed itself (SAFE-8). Repeats of the same note increment a
    /// count instead of adding an entry, and once [`MAX_TRACE_DISARMS`] distinct notes are held a new one
    /// is dropped and counted rather than growing the map without bound.
    pub fn note_disarm(&mut self, note: String) {
        if let Some(n) = self.disarms.get_mut(&note) {
            *n += 1;
        } else if self.disarms.len() < MAX_TRACE_DISARMS {
            self.disarms.insert(note, 1);
        } else {
            self.disarms_dropped += 1;
        }
    }

    /// Empty the buffer, as `debug.get_traces {clear: true}` does.
    ///
    /// **The disarm notes go with the snapshots, and that is the invariant.** A note says *this stop point
    /// stopped recording, so the silence after it is not "no more hits"* — it is an account of a gap in a
    /// buffer, and kept past the buffer it explains it describes records nobody can look at. The dropped
    /// counter goes for the same reason.
    ///
    /// [`filed`](Self::filed) is deliberately **not** reset: it is what a snapshot's `#seq` is, and
    /// restarting it would hand two different records the same number in one session — including two a
    /// **fold** in flight is still pointing at. Clearing empties what is held; it does not un-happen the
    /// hits.
    pub fn clear(&mut self) {
        self.held.clear();
        self.disarms.clear();
        self.disarms_dropped = 0;
    }

    /// What a traced hit is, with respect to chains already being tracked (EXC-3).
    ///
    /// Private since CLEAN-6: [`Self::push`] is its only caller and the seq it takes has to be the one
    /// that call just assigned. A second caller passing a seq of its own is how the fold would come to
    /// point at a record that does not exist.
    fn classify_throw(
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
}

impl Default for TraceBuffer {
    fn default() -> Self {
        Self::new()
    }
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
/// It describes the whole session surface, not this `impl` alone: #189 closed the collections into
/// [`SessionState`], [`EventRing`], [`Suspensions`], [`TraceBuffer`], [`MonitorClock`] and
/// [`Redefinitions`], so a method following one of these verbs is far likelier to be added to one of those
/// than here. This is still the block a reader lands on first.
///
/// - **`note_…`** — record that something happened, so a later reply can report it. The session is not
///   acting; it is remembering. [`Redefinitions::note_swap`], [`Redefinitions::note_pop`],
///   [`EventRing::note_watchdog`], `note_disarmed_traced`, [`TraceBuffer::note_disarm`].
/// - **`mark_…`** — set a session state that something else will read as a fact about the VM.
///   [`Suspensions::mark_suspended`], [`Suspensions::mark_resumed`].
/// - **`open` / `close`** — one half of a pair's lifetime, where the other half completes it.
///   [`MonitorClock::open`], [`MonitorClock::close`].
/// - **`hold_…` / `release_…`** — the same pairing for something the *caller* opens and closes, where the
///   debuggee is what is being held. [`Suspensions::hold_thread`], [`Suspensions::release_thread`].
/// - **`forget_…`** — drop bookkeeping that can no longer come true, which is not the same as the caller
///   closing it and must never decrement or count anything: either the debuggee contradicted the claim
///   (ADR-0003) or the request that would have completed it is gone. [`Suspensions::forget_thread`],
///   [`Suspensions::forget_all_threads`], [`MonitorClock::forget_pair`].
/// - **`push_…`** — append to a **bounded** ring buffer, with eviction counted rather than silent.
///   [`EventRing::push`], [`TraceBuffer::push`].
/// - **`clear`** — discard what a caller has read. **What survives is decided per type and the three here
///   do not agree**, so read the method before adding a fourth. The monotonic counters always survive
///   ([`EventRing::seq`], [`TraceBuffer::filed`]) because each is an identity already handed out. The drop
///   counts do not: [`EventRing::clear`] keeps its own as the only evidence the ring fell behind, while
///   [`TraceBuffer::clear`] and [`MonitorClock::clear`] reset theirs — a drop count explains a gap in
///   something a caller can still ask about, and after those two there is nothing left for it to be about.
/// - **`register_…`** — add to a collection under an id the value itself carries. `register_stop_point`.
/// - Everything else is a query and reads as one: `was_traced_and_disarmed`,
///   [`EventRing::watchdog_note_for`], `classify_throw`, `next_stop_id`.
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
/// build a whole [`DebugSession`] around. It is the workaround ADR-0050 later generalised into a type: the
/// mirror test that used to sit beside this one — a reimplementation of [`TraceBuffer::note_disarm`] in a
/// test body, for exactly the same reason — is gone, because cluster 2 gave the real method somewhere to be
/// called from and cluster 4 deleted the copy.
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
/// See [`MonitorClock::pending`] for why the pair is in the key rather than left out as redundant — a
/// `wait` re-acquires its monitor on wake, so one thread can have both pairs open on one object at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorPairKey {
    pub thread: u64,
    /// The monitor object. A **weak** reference like every object id here (ADR-0022) — it is only ever
    /// compared, never dereferenced, so a collected monitor costs an unmatched entry rather than an error.
    pub monitor: u64,
    pub pair: MonitorPair,
}

/// The debugger's own stopwatch across monitor pairs (DUMP-7, ADR-0035), and the fifth cluster ADR-0050
/// describes (CLEAN-6, #189).
///
/// **It exists because no monitor event carries an elapsed time.** `MONITOR_CONTENDED_ENTERED` reports that
/// a thread got the lock and says nothing about how long it waited; `MONITOR_WAIT` carries the timeout the
/// caller *asked* for, not what it got. "How long was it blocked" — the question a contention diagnosis is
/// actually asking — is on neither half, so the only way to have it is to timestamp the opening half here
/// and subtract on the closing one. Every reply that prints the result says the debugger measured it.
///
/// **The invariant that chose this cluster: an unclosed half must never be handed to a later request.**
/// Three rules move together, and each of the three was a line in a handler with a paragraph above it
/// explaining the same hazard — clearing a monitor stop point drops its pair's halves, re-arming one drops
/// them too, and `debug.panic` drops all of them. Left behind, any of those hands a stale start to the next
/// stop point armed on this session, which then reports the time that stop point spent DISABLED as time a
/// thread spent blocked: a number that is not wrong by a little. [`Self::forget_pair`] is that rule once.
///
/// **It is constructible with no socket**, which is the point of the type (ADR-0050). The eviction rule
/// below runs backwards from every other bound in this file and had no test at all before, because
/// reaching it against a real JVM needs 256 threads to die blocked.
#[derive(Debug)]
pub struct MonitorClock {
    /// Halves still waiting for their other half, and the instant each arrived.
    ///
    /// Bounded by [`MAX_MONITOR_PENDING`], because entries are removed by the *closing* half and there is
    /// no guarantee one ever arrives: a thread can die blocked, and arming only the opening half of a pair
    /// is a legitimate (cheaper) way to use this.
    ///
    /// **The key includes which pair.** `Object.wait()` releases its monitor and re-acquires it on wake,
    /// and that re-acquisition can itself be contended — so one thread can legitimately have a
    /// `Blocked`→`Acquired` and a `Wait`→`Waited` measurement outstanding on the *same* monitor at the same
    /// time. Keyed on (thread, monitor) alone they would overwrite each other and report one duration as
    /// the other.
    pub pending: HashMap<MonitorPairKey, std::time::Instant>,
    /// How many opening halves eviction took because [`pending`](Self::pending) was full.
    ///
    /// **Counted, and read by nothing.** [`MAX_MONITOR_PENDING`]'s doc says the eviction is counted "so a
    /// duration that goes missing this way is explicable" — the counting is real and the explaining is not:
    /// no reply, no listing and no investigation report prints this figure, so an evicted measurement is
    /// exactly as silent as an unbounded map would have made it. CLEAN-6 is a refactor and must not change
    /// a reply, so this is recorded rather than fixed; every other bounded buffer here reports its drops,
    /// and this one is the exception.
    pub dropped: u64,
}

impl MonitorClock {
    /// A session's monitor stopwatch at attach: nothing open, nothing lost.
    #[must_use]
    pub fn new() -> Self {
        Self { pending: HashMap::new(), dropped: 0 }
    }

    /// Record that the **opening** half of a pair has arrived, so the closing half can measure the
    /// duration (DUMP-7, ADR-0035).
    ///
    /// An opening half arriving twice for the same key **overwrites** rather than being ignored. That is
    /// the honest reading: JDWP delivered a second "started blocking" without a matching "acquired", so
    /// either the first pair's close was never armed or the event was lost, and measuring the newer pair is
    /// right where measuring from a stale start would report a duration that includes work the thread was
    /// not blocked for.
    ///
    /// **At the bound it evicts the OLDEST rather than refusing the new one**, which is the opposite of
    /// every other bound in this file and is deliberate — see [`MAX_MONITOR_PENDING`]. Refusing would be
    /// self-defeating: the way this map fills is with halves that will never close, so a refusal would stop
    /// measuring durations *permanently* the first time 256 threads died blocked.
    pub fn open(&mut self, key: MonitorPairKey, at: std::time::Instant) {
        if self.pending.len() >= MAX_MONITOR_PENDING && !self.pending.contains_key(&key) {
            if let Some(oldest) = self.pending.iter().min_by_key(|(_, t)| **t).map(|(k, _)| *k) {
                self.pending.remove(&oldest);
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.pending.insert(key, at);
    }

    /// Close a pair, returning how long it was open — the **debugger-measured** duration, since no monitor
    /// event carries one.
    ///
    /// `None` when this closing half has no matching opening half, which is a normal state rather than an
    /// error and has three innocent causes: the opening kind was never armed, the pair opened before this
    /// stop point was, or the entry was evicted. A reply that got `None` must say the duration is
    /// unavailable rather than print a zero, which would read as "it was not blocked at all".
    pub fn close(&mut self, key: &MonitorPairKey, now: std::time::Instant) -> Option<std::time::Duration> {
        let opened = self.pending.remove(key)?;
        Some(now.saturating_duration_since(opened))
    }

    /// Drop every open half of **one** pair kind, because the request that opened them is gone — the stop
    /// point was cleared, or re-armed onto a new request id.
    ///
    /// **The other pair kind is untouched, and that is the whole reason this takes an argument.** A session
    /// can have both pairs armed on the same threads and the same monitors at once ([`MonitorPairKey`]),
    /// and clearing the contended stop point says nothing about a `wait` measurement in flight.
    ///
    /// It does **not** count what it drops. An eviction is this type failing to keep a measurement it was
    /// asked to keep; this is a measurement whose request no longer exists, so there is nothing to explain
    /// to a caller and nothing that would have completed it.
    pub fn forget_pair(&mut self, pair: MonitorPair) {
        self.pending.retain(|k, _| k.pair != pair);
    }

    /// Drop every open half and the eviction count with it — `debug.panic`'s case, where every armed
    /// request has just been dropped.
    ///
    /// **The count goes, unlike [`EventRing::clear`]'s**, for the reason [`TraceBuffer::clear`]'s does: a
    /// drop count explains a gap in something a caller can still ask about, and after this there is no
    /// measurement left for it to be about.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.dropped = 0;
    }
}

impl Default for MonitorClock {
    fn default() -> Self {
        Self::new()
    }
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
            events: EventRing::new(),
            event_listener_task: None,
            last_thread: None,
            pending_step: None,
            suspensions: Suspensions::new(),
            watchdog_task: None,
            traces: TraceBuffer::new(),
            read_only,
            source_roots,
            class_roots,
            trace_exprs,
            step_exclude_classes,
            step_only_classes,
            redefinitions: Redefinitions::new(),
            pattern_sets: HashMap::new(),
            launched: None,
            monitor_clock: MonitorClock::new(),
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

    /// A snapshot in the state these tests need it and nothing more — a **stated** record, authored here
    /// rather than captured, the same move `pending` above makes.
    fn snapshot(class: &str) -> TraceRecord {
        TraceRecord {
            // Both are assigned by `TraceBuffer::push`. Deliberately wrong here, so a push that failed to
            // stamp them would be visible rather than accidentally right.
            seq: u64::MAX,
            rethrow: None,
            bp_id: "exc_1".to_string(),
            thread: 0x1f4c,
            class: class.to_string(),
            method: "reservar".to_string(),
            line: Some(412),
            args: Vec::new(),
            captured: Vec::new(),
            callers: Vec::new(),
            expr: Vec::new(),
            detail: Vec::new(),
        }
    }

    /// CLEAN-6 (#189): the seq is the buffer's to assign, and it counts what was FILED rather than what
    /// is held.
    ///
    /// The difference is what `debug.get_traces` and the investigation report print as "N are no longer
    /// here", and it is the one number a reader has to tell a quiet trace from a buffer that overflowed.
    /// Unassertable before this cluster moved: filing a record needed the event pump, and the event pump
    /// needs a socket (ADR-0049).
    #[test]
    fn a_filed_snapshot_is_stamped_with_the_count_of_everything_ever_filed() {
        let mut buffer = TraceBuffer::new();
        for i in 1..=3 {
            buffer.push(snapshot("Pedido"), 1, 0x1f4c, None);
            assert_eq!(buffer.filed, i, "every push counts, whatever the ring does with it");
        }
        let seqs: Vec<u64> = buffer.held.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3], "the record carries the seq the buffer gave it, in arrival order");
    }

    /// The ring evicts the oldest and the counter does not follow it down.
    ///
    /// `filed - held.len()` is what a reader cannot see any more, so a counter reset by eviction would
    /// report a full buffer as a complete one — the silence-reads-as-an-answer failure this repo is built
    /// against.
    #[test]
    fn an_overflowing_buffer_drops_the_oldest_and_still_says_how_many_it_took() {
        let mut buffer = TraceBuffer::new();
        for _ in 0..MAX_TRACES + 5 {
            buffer.push(snapshot("Pedido"), 1, 0x1f4c, None);
        }
        assert_eq!(buffer.held.len(), MAX_TRACES, "the ring is bounded");
        assert_eq!(
            buffer.filed,
            MAX_TRACES as u64 + 5,
            "the count is of what was filed, not of what is held"
        );
        assert_eq!(
            buffer.held.front().map(|r| r.seq),
            Some(6),
            "the five oldest are the ones evicted, and the survivors keep the seqs they were filed with"
        );
    }

    /// EXC-3: one exception instance rethrown many times keeps both ends and collapses the middle.
    ///
    /// **The assertion with teeth is that the middle is gone from the RING**, not merely marked. A fold
    /// that annotated the record without evicting the sighting it supersedes would leave the buffer full
    /// of interceptor plumbing — the 30-of-38 capture that #68 was filed about — with every test still
    /// green, because the note would be there and the count would be right.
    #[test]
    fn a_rethrown_instance_keeps_its_first_and_latest_sighting_and_folds_the_middle() {
        let mut buffer = TraceBuffer::new();
        let (req, thread, exc) = (7, 0x1f4c, 0x9999);

        assert!(
            matches!(buffer.push(snapshot("Throwing"), req, thread, Some(exc)), ThrowKind::First),
            "the first sighting of an instance is a first throw and is charged"
        );
        for _ in 0..4 {
            assert!(
                matches!(
                    buffer.push(snapshot("Interceptor"), req, thread, Some(exc)),
                    ThrowKind::Rethrow { .. }
                ),
                "every later sighting of the same instance on the same thread is a rethrow"
            );
        }

        assert_eq!(buffer.filed, 5, "a fold does not stop a record being filed");
        assert_eq!(buffer.held.len(), 2, "only the first throw and the latest sighting survive in the ring");
        let seqs: Vec<u64> = buffer.held.iter().map(|r| r.seq).collect();
        assert_eq!(
            seqs,
            vec![1, 5],
            "the two ends, and the rolling latest is the one that converges on the escape"
        );

        let fold = buffer.held.back().and_then(|r| r.rethrow).expect("the escaping record carries the fold");
        assert_eq!(
            fold.first_seq, 1,
            "the fold points at the original throw, which has the application frame"
        );
        assert_eq!(fold.collapsed, 3, "three of the four rethrows were collapsed; the fourth IS this record");
    }

    /// A different instance is not the same chain, and neither is the same instance on another thread.
    ///
    /// The thread is in the key precisely because JDWP object ids are reusable, so this is the assertion
    /// that a later unrelated exception handed a recycled id is not folded into a dead chain.
    #[test]
    fn a_chain_is_keyed_by_request_thread_and_instance_together() {
        let mut buffer = TraceBuffer::new();
        buffer.push(snapshot("A"), 7, 0x1f4c, Some(0x9999));

        for (req, thread, exc, why) in [
            (7, 0x1f4c, 0xAAAA, "a different instance"),
            (7, 0x2222, 0x9999, "the same id on another thread"),
            (8, 0x1f4c, 0x9999, "the same id from another stop point"),
        ] {
            assert!(
                matches!(buffer.push(snapshot("B"), req, thread, Some(exc)), ThrowKind::First),
                "{why} must start its own chain, not fold into the first"
            );
        }
        assert_eq!(buffer.held.len(), 4, "nothing was superseded, so every record is still held");
    }

    /// A hit with no exception is never a rethrow — a line breakpoint or a watchpoint has no instance to
    /// chain, and folding one would collapse unrelated hits of the same stop point into a count.
    #[test]
    fn a_hit_with_no_exception_is_always_a_first_throw() {
        let mut buffer = TraceBuffer::new();
        for _ in 0..3 {
            assert!(matches!(buffer.push(snapshot("Pedido"), 1, 0x1f4c, None), ThrowKind::First));
        }
        assert_eq!(buffer.held.len(), 3, "three hits of one logpoint are three snapshots");
    }

    /// SAFE-8: repeats of a disarm note collapse into a count, and the map is bounded.
    ///
    /// **This used to have a mirror beside it** — a `note_into` helper reimplementing
    /// `TraceBuffer::note_disarm` in the test body, asserting on the reimplementation, and unable to fail
    /// on any change to the real method. It existed because a `DebugSession` needed a socket (ADR-0049);
    /// cluster 2 removed that reason and cluster 4 removed the test, which is the shape ADR-0050 exists to
    /// delete rather than to accumulate.
    #[test]
    fn disarm_notes_collapse_and_the_map_is_bounded() {
        let mut buffer = TraceBuffer::new();
        for _ in 0..12 {
            buffer.note_disarm("watch_3 stopped recording".to_string());
        }
        assert_eq!(buffer.disarms.len(), 1, "a repeat is a count, not another entry");
        assert_eq!(buffer.disarms.get("watch_3 stopped recording"), Some(&12));
        assert_eq!(buffer.disarms_dropped, 0, "nothing was dropped while there was room");

        for i in 0..MAX_TRACE_DISARMS + 5 {
            buffer.note_disarm(format!("bp_{i} stopped recording"));
        }
        assert_eq!(buffer.disarms.len(), MAX_TRACE_DISARMS, "the map stays bounded");
        assert_eq!(buffer.disarms_dropped, 6, "and every note it could not hold is COUNTED, not silent");
    }

    /// `debug.get_traces {clear: true}` empties the buffer and the notes together, and leaves the count.
    ///
    /// **Both halves are the invariant.** A note kept past the snapshots it explains describes records
    /// nobody can look at; a `filed` reset would hand two records in one session the same `#seq`,
    /// including records a fold still in flight is pointing at.
    #[test]
    fn clearing_takes_the_disarm_notes_with_it_and_leaves_the_filed_count() {
        let mut buffer = TraceBuffer::new();
        buffer.push(snapshot("Pedido"), 1, 0x1f4c, None);
        buffer.push(snapshot("Pedido"), 1, 0x1f4c, None);
        buffer.note_disarm("bp_1 stopped recording".to_string());

        buffer.clear();

        assert!(buffer.held.is_empty(), "the snapshots go");
        assert!(buffer.disarms.is_empty(), "and so do the notes that explain gaps between them");
        assert_eq!(buffer.disarms_dropped, 0);
        assert_eq!(buffer.filed, 2, "clearing empties what is held; it does not un-happen the hits");

        buffer.push(snapshot("Pedido"), 1, 0x1f4c, None);
        assert_eq!(
            buffer.held.front().map(|r| r.seq),
            Some(3),
            "so the next snapshot cannot reuse a number an earlier one had"
        );
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

    // SAFE-2/SAFE-4: the request id is part of the cause's identity, because the watchdog disarms the
    // request the cause names. Two stop-point causes that compared equal would let it disarm the wrong one.
    #[test]
    fn suspend_cause_distinguishes_a_stop_point_from_a_manual_pause() {
        assert_ne!(SuspendCause::ManualPause, SuspendCause::StopPoint(7));
        assert_eq!(SuspendCause::StopPoint(7), SuspendCause::StopPoint(7));
        assert_ne!(SuspendCause::StopPoint(7), SuspendCause::StopPoint(8));
    }

    /// `secs` seconds after `t0`, for the watchdog assertions below.
    ///
    /// **Forward from an origin the test owns, never backwards from now.** Building an age with
    /// `Instant::now() - 300s` would make these tests depend on how long the machine has been up:
    /// `Instant` has no portable origin, and `checked_sub` answers `None` when the result would precede
    /// it — a panic on a box that booted a minute ago, and a `windows-latest` or `macos-latest`
    /// contributor is who would hit it. `handlers.rs`'s `jvm_method` helper refuses the same trick for
    /// the same reason, and it is why `hold_thread` and `overdue_threads` take their instant rather than
    /// reading the clock themselves, the way [`MonitorClock::open`] already does.
    fn secs_after(t0: std::time::Instant, secs: u64) -> std::time::Instant {
        t0 + std::time::Duration::from_secs(secs)
    }

    // SAFE-4/SAFE-5: the two halves of "the VM is suspended" cannot come apart, because they are one
    // value. Tracking them separately is what let a manual pause record a clock and no cause at all — so
    // the watchdog resumed the VM and could not say what had frozen it.
    #[test]
    fn a_vm_suspension_carries_its_clock_and_its_cause_together() {
        let mut s = Suspensions::new();
        assert!(s.vm.is_none(), "a fresh session's debuggee is running");

        s.mark_suspended(SuspendCause::StopPoint(11));
        let vm = s.vm.expect("suspended");
        assert_eq!(vm.cause, SuspendCause::StopPoint(11));
        assert!(vm.since.elapsed() < std::time::Duration::from_secs(5), "the clock started just now");

        s.mark_resumed();
        assert!(s.vm.is_none(), "a resume clears the cause with the clock, never one without the other");
    }

    // SAFE-11, and the reason `Suspensions` holds both fields: a held thread is not a suspended VM, and a
    // suspended VM does not release a held thread. Collapsing them would make `debug.list_sessions` print
    // SUSPENDED about a VM serving requests normally, and make `debug.continue` look like it freed a worker.
    #[test]
    fn a_suspended_vm_and_a_held_thread_are_independent_facts() {
        let mut s = Suspensions::new();

        s.hold_thread(0x1f, "http-worker-3".to_string(), std::time::Instant::now());
        assert!(s.vm.is_none(), "holding one worker does not stop the VM");

        s.mark_suspended(SuspendCause::ManualPause);
        assert_eq!(s.threads.len(), 1, "suspending the VM does not change what we hold thread-wise");

        s.mark_resumed();
        assert!(
            s.threads.contains_key(&0x1f),
            "debug.continue clears the VM's depth, which is a different count — the worker is still held"
        );
    }

    // The invariant `hold_thread` exists to hold rather than to describe: the watchdog measures a thread's
    // age from the FIRST suspend, so a caller cannot keep a worker frozen forever by suspending it again.
    #[test]
    fn a_second_suspend_of_one_thread_does_not_restart_its_clock() {
        let mut s = Suspensions::new();
        let t0 = std::time::Instant::now();
        let five_min_on = secs_after(t0, 300);
        assert_eq!(s.hold_thread(0x2a, "pool-1-thread-4".to_string(), t0), 1);

        assert_eq!(
            s.hold_thread(0x2a, "pool-1-thread-4".to_string(), five_min_on),
            2,
            "our own claim count grows"
        );
        assert_eq!(s.threads[&0x2a].since, t0, "the clock the watchdog reads must not move");
        assert_eq!(s.threads.len(), 1, "and it is still one thread, not two records");
        assert_eq!(
            s.overdue_threads(120, five_min_on),
            vec![0x2a],
            "so it is still overdue five minutes on, which is the rescue this protects"
        );
    }

    // ADR-0003, one call one decrement: a caller who suspended twice is told they are one call short
    // rather than told they succeeded. The record has to survive the first release for the reply to say so.
    #[test]
    fn a_thread_held_twice_needs_two_releases() {
        let mut s = Suspensions::new();
        let now = std::time::Instant::now();
        s.hold_thread(0x33, "scheduler".to_string(), now);
        s.hold_thread(0x33, "scheduler".to_string(), now);

        let first = s.release_thread(0x33).expect("we were holding it");
        assert_eq!(first.issued, 2, "the record as it stood, so the reply can name a claim it is dropping");
        assert_eq!(s.threads[&0x33].issued, 1, "one claim left, so this session still holds the thread");

        let second = s.release_thread(0x33).expect("still held");
        assert_eq!(second.issued, 1);
        assert!(!s.threads.contains_key(&0x33), "our count reached zero, so the claim goes");
    }

    // ADR-0003's other half, and the distinction between the two verbs. `forget_thread` drops the whole
    // claim because the debuggee has contradicted it — a thread that ended, or one a debug.panic resumed
    // to zero. Decrementing there would leave a record for a thread nothing is holding, which the watchdog
    // would then try to rescue and `list_sessions` would report as frozen.
    #[test]
    fn the_debuggees_answer_forgets_a_claim_our_own_count_still_believes_in() {
        let mut s = Suspensions::new();
        let now = std::time::Instant::now();
        s.hold_thread(0x44, "doomed".to_string(), now);
        s.hold_thread(0x44, "doomed".to_string(), now);

        assert!(s.forget_thread(0x44), "there was a claim to drop");
        assert!(s.threads.is_empty(), "the whole claim goes, not one of its two suspends");
        assert!(!s.forget_thread(0x44), "and a second call has nothing to drop");
    }

    // Neither verb may panic on a thread this session never held: `debug.resume_thread` reaches both with
    // a caller-supplied id, and `verify_thread_suspends` reaches `forget_thread` for a thread the JVM has
    // already let go.
    #[test]
    fn releasing_or_forgetting_a_thread_nobody_held_says_so() {
        let mut s = Suspensions::new();
        assert!(s.release_thread(0x99).is_none());
        assert!(!s.forget_thread(0x99));
        assert!(s.threads.is_empty(), "and neither invented a record on the way");
    }

    // SAFE-11's watchdog arm: only the OVERDUE threads. A thread suspended ten seconds ago is a caller at
    // work, not a leak. Paired with the clock invariant above — a repeatedly suspended thread stays overdue,
    // which is the span between `hold_thread` and `overdue_threads` that neither can be checked apart from.
    #[test]
    fn only_threads_past_the_watchdogs_bound_are_overdue() {
        let mut s = Suspensions::new();
        let t0 = std::time::Instant::now();
        let now = secs_after(t0, 300);
        s.hold_thread(0x1, "stale-worker".to_string(), t0);
        s.hold_thread(0x2, "recent-worker".to_string(), secs_after(t0, 290));

        assert_eq!(s.overdue_threads(120, now), vec![0x1], "the ten-second-old one is a caller at work");
        assert!(s.overdue_threads(600, now).is_empty(), "nothing is overdue against a longer bound");

        s.hold_thread(0x1, "stale-worker".to_string(), now);
        assert_eq!(
            s.overdue_threads(120, now),
            vec![0x1],
            "suspending it again must not buy it another 120 seconds — that is the rescue never firing"
        );
    }

    // The "held up to" figure a rescue note prints is the OLDEST of the threads being released, and it is
    // measured over the ids asked about rather than over everything held: the watchdog releases a subset.
    #[test]
    fn the_rescue_note_measures_the_oldest_of_the_threads_it_names() {
        let mut s = Suspensions::new();
        let t0 = std::time::Instant::now();
        let now = secs_after(t0, 300);
        s.hold_thread(0x1, "oldest".to_string(), t0);
        s.hold_thread(0x2, "middle".to_string(), secs_after(t0, 100));
        s.hold_thread(0x3, "newest".to_string(), secs_after(t0, 299));

        assert_eq!(
            s.longest_held(&[0x2, 0x3], now),
            std::time::Duration::from_secs(200),
            "the oldest of the two NAMED, not the 300s thread nobody asked about"
        );
        assert_eq!(
            s.longest_held(&[], now),
            std::time::Duration::ZERO,
            "and nothing named is zero rather than a panic on an empty max"
        );
    }

    // `debug.disconnect`'s case: Dispose has already resumed every thread, so every claim goes at once and
    // the reply names them. In the map's stable order, because a caller comparing two disconnect replies
    // should not see a hash order shuffle.
    #[test]
    fn forgetting_every_thread_names_them_in_a_stable_order() {
        let mut s = Suspensions::new();
        let now = std::time::Instant::now();
        s.hold_thread(0x30, "ajp-3".to_string(), now);
        s.hold_thread(0x10, "ajp-1".to_string(), now);
        s.hold_thread(0x20, "ajp-2".to_string(), now);

        assert_eq!(s.forget_all_threads(), vec!["ajp-1", "ajp-2", "ajp-3"], "ordered by thread id");
        assert!(s.threads.is_empty());
        assert!(s.forget_all_threads().is_empty(), "and a second disconnect has nothing left to name");
    }

    /// A **stated** event set: the ring's invariants are about the accounting around a record and never
    /// about what is inside one — and `Event`'s wire discriminant is `pub(crate)` to `jdwp-client`
    /// besides, so there is nothing to author here even if it mattered.
    fn arrival() -> EventSet {
        EventSet { suspend_policy: 2, events: Vec::new() }
    }

    /// A note this ring is holding, so a test can ask what it is scoped to.
    fn rescue() -> String {
        "watchdog auto-resumed the VM after 300s and disabled bp_1".to_string()
    }

    #[test]
    fn every_event_is_numbered_in_arrival_order_and_the_pusher_is_told_which() {
        let mut ring = EventRing::new();

        assert_eq!(ring.push(arrival(), None), 1, "the first event is #1, not #0");
        assert_eq!(ring.push(arrival(), None), 2);
        assert_eq!(ring.push(arrival(), None), 3, "the returned seq is the one the caller notifies with");

        assert_eq!(
            ring.held.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the record carries the same number that was handed back, oldest first"
        );
        assert_eq!(ring.dropped, 0, "nothing was dropped while there was room");
    }

    /// An evicted event does **not** give its number back, which is why this asserts the surviving
    /// *numbers* and not just the surviving count: a ring evicting from the wrong end holds exactly as
    /// many records and starts them at #1.
    #[test]
    fn an_overflowing_ring_drops_the_oldest_and_never_reissues_its_number() {
        let mut ring = EventRing::new();
        let over = 5;
        for _ in 0..MAX_EVENTS + over {
            ring.push(arrival(), None);
        }

        assert_eq!(ring.held.len(), MAX_EVENTS, "the ring stays bounded");
        assert_eq!(ring.dropped, over as u64, "and every eviction is COUNTED, not silent");
        assert_eq!(
            ring.held.front().map(|r| r.seq),
            Some(over as u64 + 1),
            "the oldest SURVIVOR is numbered past the events that were evicted"
        );
        assert_eq!(
            ring.held.back().map(|r| r.seq),
            Some((MAX_EVENTS + over) as u64),
            "and the newest carries the count of everything that ever arrived"
        );
        assert_eq!(ring.tail(1).dropped, over as u64, "a reply reads the gap off the tail it renders");
    }

    /// The tail's `unshown` must come from the same clamped count its slice did — recomputed against the
    /// caller's raw `limit` it would name events that are already on screen (see [`EventRing::tail`]).
    #[test]
    fn a_tail_shows_the_newest_and_says_how_many_older_ones_it_held_back() {
        let mut ring = EventRing::new();
        for _ in 0..5 {
            ring.push(arrival(), None);
        }

        let two = ring.tail(2);
        assert_eq!(two.shown.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![4, 5], "newest LAST");
        assert_eq!(two.unshown, 3, "the three older ones are what a larger limit would reach");

        let bare = ring.tail(0);
        assert_eq!(
            bare.shown.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![5],
            "a bare get_last_event means the latest event, as it always has"
        );
        assert_eq!(bare.unshown, 4, "and the count held back follows the clamp, not the caller's zero");

        let everything = ring.tail(99);
        assert_eq!(everything.shown.len(), 5, "a limit past the end shows the whole ring");
        assert_eq!(everything.unshown, 0, "with nothing left to catch up on");
    }

    /// SAFE-10 and its watermark: a drain is the caller saying they have read the events, not that the
    /// numbering restarts or that the rescue never happened.
    #[test]
    fn draining_keeps_the_sequence_the_drop_count_and_the_rescue_note() {
        let mut ring = EventRing::new();
        for _ in 0..MAX_EVENTS + 2 {
            ring.push(arrival(), None);
        }
        ring.note_watchdog(rescue());

        ring.clear();

        assert!(ring.held.is_empty(), "the events a caller has read are gone");
        assert_eq!(ring.dropped, 2, "the drop count is the only evidence the ring ever fell behind");
        assert_eq!(
            ring.push(arrival(), None),
            (MAX_EVENTS + 3) as u64,
            "and the next event carries the NEXT number — a reused one would name two events"
        );
        assert_eq!(
            ring.watchdog_note.as_deref(),
            Some(rescue().as_str()),
            "the rescue survives, because list_stop_points is where a caller who walked away reads it"
        );
        // Asserted separately from the note, because a clear that kept only ONE half leaves the note
        // unrenderable rather than absent: `watchdog_note_for` answers `None` for every event forever, so
        // `get_last_event` silently stops carrying `[watchdog] this suspension has since ended`.
        assert_eq!(
            ring.watchdog_seq,
            Some((MAX_EVENTS + 2) as u64),
            "and so does the watermark that decides which event it is rendered against"
        );
    }

    /// SAFE-10 (#69): the note is an account of **one** suspension ending, so it is rendered against the
    /// event that suspension belongs to and never against a later one.
    #[test]
    fn a_rescue_note_is_reported_only_against_the_suspension_it_ended() {
        let mut ring = EventRing::new();
        let hit = ring.push(arrival(), None);
        ring.note_watchdog(rescue());

        assert_eq!(
            ring.watchdog_note_for(Some(hit)),
            Some(rescue().as_str()),
            "the caller looking at the suspension the watchdog ended is the one who must be told"
        );

        let fresh = ring.push(arrival(), None);
        assert_eq!(
            ring.watchdog_note_for(Some(fresh)),
            None,
            "a live hit must not be captioned with a rescue that happened before it"
        );
        assert_eq!(
            ring.watchdog_note_for(Some(hit)),
            Some(rescue().as_str()),
            "and reading the older event back still gets its own history"
        );
        assert_eq!(ring.watchdog_note_for(None), None, "with nothing rendered there is nothing to caption");
        assert_eq!(
            EventRing::new().watchdog_note_for(Some(1)),
            None,
            "a ring the watchdog never rescued has no note to scope"
        );
    }

    /// A key naming one measurement. Distinct `thread`s give distinct keys, which is all these tests need
    /// of the identity — the *pair* half of it has a test of its own.
    fn blocked_on(thread: u64) -> MonitorPairKey {
        MonitorPairKey { thread, monitor: 0xBEEF, pair: MonitorPair::Contended }
    }

    /// DUMP-7: the debugger is the only source of an elapsed time here, so the pair has to measure from
    /// the half that opened it and answer honestly when there was no such half.
    #[test]
    fn a_closed_pair_measures_from_its_opening_half_and_an_unmatched_one_measures_nothing() {
        let mut clock = MonitorClock::new();
        let origin = std::time::Instant::now();

        clock.open(blocked_on(7), origin);
        assert_eq!(
            clock.close(&blocked_on(7), secs_after(origin, 4)),
            Some(std::time::Duration::from_secs(4)),
            "the duration is measured across the two halves, because no monitor event carries one"
        );
        assert!(clock.pending.is_empty(), "and the closing half takes the entry with it");

        assert_eq!(
            clock.close(&blocked_on(7), secs_after(origin, 9)),
            None,
            "a closing half with no opening half reports nothing — never a zero, which would read as \
             'it was not blocked at all'"
        );
    }

    /// A `wait` releases its monitor and re-acquires it on wake, and that re-acquisition can itself be
    /// contended — so one thread can have both pairs open on one object at once. Keyed without the pair
    /// they would overwrite each other and report one duration as the other.
    #[test]
    fn both_pairs_of_one_thread_and_monitor_are_measured_independently() {
        let mut clock = MonitorClock::new();
        let origin = std::time::Instant::now();
        let waiting = MonitorPairKey { pair: MonitorPair::Wait, ..blocked_on(7) };

        clock.open(blocked_on(7), origin);
        clock.open(waiting, secs_after(origin, 3));

        assert_eq!(
            clock.close(&waiting, secs_after(origin, 5)),
            Some(std::time::Duration::from_secs(2)),
            "the wait is measured from ITS own opening half"
        );
        assert_eq!(
            clock.close(&blocked_on(7), secs_after(origin, 6)),
            Some(std::time::Duration::from_secs(6)),
            "and the contended pair is still open and still measuring from its own"
        );
    }

    /// A second opening half for one key **overwrites**: JDWP delivered a "started blocking" with no
    /// matching "acquired", so measuring from the stale start would report work the thread was not
    /// blocked for.
    #[test]
    fn a_repeated_opening_half_measures_from_the_newer_one() {
        let mut clock = MonitorClock::new();
        let origin = std::time::Instant::now();

        clock.open(blocked_on(7), origin);
        clock.open(blocked_on(7), secs_after(origin, 10));

        assert_eq!(clock.pending.len(), 1, "one key is one measurement, however many halves arrived");
        assert_eq!(
            clock.close(&blocked_on(7), secs_after(origin, 12)),
            Some(std::time::Duration::from_secs(2)),
            "measured from the newer start, not the stale one"
        );
    }

    /// **This bound runs backwards from every other one in this file** — it evicts rather than refusing —
    /// and reaching it against a real JVM needs 256 threads to die blocked, which is why it had no test
    /// until the type could be built without a socket (ADR-0050).
    #[test]
    fn a_full_clock_evicts_its_oldest_measurement_rather_than_refusing_the_new_one() {
        let mut clock = MonitorClock::new();
        let origin = std::time::Instant::now();

        for t in 0..u64::try_from(MAX_MONITOR_PENDING).unwrap_or(u64::MAX) {
            clock.open(blocked_on(t), secs_after(origin, t));
        }
        assert_eq!(clock.pending.len(), MAX_MONITOR_PENDING);
        assert_eq!(clock.dropped, 0, "nothing was lost while there was room");

        let newcomer = blocked_on(9_000);
        clock.open(newcomer, secs_after(origin, 9_000));

        assert_eq!(clock.pending.len(), MAX_MONITOR_PENDING, "the map stays bounded");
        assert_eq!(clock.dropped, 1, "and the eviction is counted");
        assert!(
            clock.pending.contains_key(&newcomer),
            "the NEW half is kept — refusing it would stop this session measuring durations forever"
        );
        assert!(
            !clock.pending.contains_key(&blocked_on(0)),
            "and the one evicted is the oldest, which is the least likely to still be waiting"
        );
    }

    /// A re-open of a key already present must not evict, because it is not a new entry — the guard for
    /// that is what stops a busy monitor from evicting a stranger on every repeat.
    #[test]
    fn re_opening_a_measurement_already_held_evicts_nothing() {
        let mut clock = MonitorClock::new();
        let origin = std::time::Instant::now();
        for t in 0..u64::try_from(MAX_MONITOR_PENDING).unwrap_or(u64::MAX) {
            clock.open(blocked_on(t), secs_after(origin, t));
        }

        clock.open(blocked_on(3), secs_after(origin, 500));

        assert_eq!(clock.dropped, 0, "a repeat is not an arrival, so nothing was displaced by it");
        assert_eq!(clock.pending.len(), MAX_MONITOR_PENDING);
    }

    /// Clearing a monitor stop point drops the halves of **its** pair and says nothing about the other,
    /// because a session can have both armed on the same threads at once.
    #[test]
    fn forgetting_one_pair_kind_leaves_the_other_measuring() {
        let mut clock = MonitorClock::new();
        let origin = std::time::Instant::now();
        let waiting = MonitorPairKey { pair: MonitorPair::Wait, ..blocked_on(7) };
        clock.open(blocked_on(7), origin);
        clock.open(blocked_on(8), origin);
        clock.open(waiting, origin);

        clock.forget_pair(MonitorPair::Contended);

        assert_eq!(clock.pending.len(), 1, "every contended half went, on every thread");
        assert!(clock.pending.contains_key(&waiting), "and the wait pair is untouched");
        assert_eq!(
            clock.dropped, 0,
            "a request that no longer exists is not a measurement this type failed to keep, so it is \
             NOT counted as one"
        );
    }

    /// `debug.panic` drops every armed request, so the pairing state goes with them — and the eviction
    /// count goes too, because there is no measurement left for it to explain.
    #[test]
    fn clearing_takes_the_measurements_and_the_eviction_count_together() {
        let mut clock = MonitorClock::new();
        let origin = std::time::Instant::now();
        for t in 0..=u64::try_from(MAX_MONITOR_PENDING).unwrap_or(u64::MAX) {
            clock.open(blocked_on(t), secs_after(origin, t));
        }
        assert_eq!(clock.dropped, 1, "the setup really did evict one");

        clock.clear();

        assert!(clock.pending.is_empty());
        assert_eq!(clock.dropped, 0, "unlike EventRing::clear — see the clear bullet in the vocabulary");
    }

    /// SWAP-2: an iterating caller reloads the same class over and over, and "17 times" is a different
    /// situation to report than "once" — so a repeat is a count, and the clock names the NEWEST swap,
    /// which is what "how long has this JVM been like this" is rendered from.
    #[test]
    fn a_repeated_swap_of_one_class_is_a_count_and_the_clock_names_the_newest() {
        let mut swapped = Redefinitions::new();
        let origin = std::time::Instant::now();

        swapped.note_swap("com.acme.OrderService", origin);
        swapped.note_swap("com.acme.OrderService", secs_after(origin, 60));

        assert_eq!(swapped.held.len(), 1, "a repeat is a count, not another entry");
        let rec = swapped.held.get("com.acme.OrderService").expect("the class is held");
        assert_eq!(rec.count, 2);
        assert_eq!(rec.at, secs_after(origin, 60), "the clock moved to the newest swap, not the first");
    }

    /// **The invariant this type exists to hold.** A pop applies to the bytecode that was live when it
    /// happened; a fresh swap replaced that bytecode, so whether the newest code has reached the frames
    /// still running is once again unknown — and the residue report has a different sentence for each.
    #[test]
    fn a_fresh_swap_undoes_an_earlier_pop() {
        let mut swapped = Redefinitions::new();
        let origin = std::time::Instant::now();

        swapped.note_swap("com.acme.OrderService", origin);
        swapped.note_pop("com.acme.OrderService");
        assert!(
            swapped.held["com.acme.OrderService"].popped_since,
            "the pop is recorded: this swap IS live in the frames that matter"
        );

        swapped.note_swap("com.acme.OrderService", secs_after(origin, 60));

        assert!(
            !swapped.held["com.acme.OrderService"].popped_since,
            "and the second swap un-does it — reporting otherwise would claim the NEW code is live \
             because a frame was popped under the code it replaced"
        );
    }

    /// A pop in a class this session never redefined leaves no record: there is no residue to describe,
    /// and an entry here would name a class in the residue report that this session never changed.
    #[test]
    fn a_pop_in_a_class_this_session_never_redefined_is_not_tracked() {
        let mut swapped = Redefinitions::new();

        swapped.note_pop("com.acme.NeverTouched");

        assert!(swapped.held.is_empty(), "nothing to report is reported as nothing");
    }

    #[test]
    fn a_pop_marks_only_the_class_it_names() {
        let mut swapped = Redefinitions::new();
        let origin = std::time::Instant::now();
        swapped.note_swap("com.acme.A", origin);
        swapped.note_swap("com.acme.B", origin);

        swapped.note_pop("com.acme.A");

        assert!(swapped.held["com.acme.A"].popped_since);
        assert!(
            !swapped.held["com.acme.B"].popped_since,
            "a pop is evidence about one class's frames and says nothing about another's"
        );
    }
}
