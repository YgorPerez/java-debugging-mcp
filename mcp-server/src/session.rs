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
    /// Breakpoints requested on classes not yet loaded. Each holds a `CLASS_PREPARE` request that
    /// fires when the class loads; the event pump then arms the real breakpoint. See handlers.rs.
    pub pending_breakpoints: Vec<PendingBreakpoint>,
    /// Wildcard line-breakpoint families (FILT-3), keyed by their `bpset_` id.
    pub pattern_sets: HashMap<String, PatternStopSet>,
    /// The JVM this session STARTED, if any (LAUNCH-1) — `None` for an ordinary `debug.attach`, which is
    /// the difference between a debuggee whose lifetime is ours and one that belongs to somebody else.
    pub launched: Option<LaunchedJvm>,
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

impl DebugSession {
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
    pub fn mark_resumed(&mut self) {
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

    /// Remember that a traced stop point's JDWP request was just disarmed, so hits it had already
    /// generated are still resumed rather than surfaced as suspending events (TRACE-8, #72).
    pub fn note_disarmed_traced(&mut self, req_id: i32) {
        remember_bounded(&mut self.disarmed_traced_requests, req_id, MAX_DISARMED_TRACED);
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
    fn owns_live_request(&self, req_id: i32) -> bool {
        let id = Some(req_id);
        self.breakpoints.values().any(|b| b.owns_request(req_id))
            || self.exception_requests.values().any(|e| e.request_id == id)
            || self.watchpoints.values().any(|w| w.request_id == id)
            || self.method_exits.values().any(|m| m.request_id == id)
            // A deferred breakpoint's CLASS_PREPARE is a live request too, and arming the real breakpoint
            // when it fires is not something to skip.
            || self.pending_breakpoints.iter().any(|p| p.class_prepare_request_id == req_id)
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
    /// (expression, rendered result) when the logpoint had a trace expression.
    pub expr: Option<(String, String)>,
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
    /// Set when the **debuggee** removed this request rather than the caller (FILT-8). See
    /// [`BreakpointInfo::spent`].
    pub spent: bool,
    /// The `Count` modifier this request was armed with, kept so a re-arm reproduces it (FILT-8).
    pub hit_count: Option<i32>,
    /// How many throws the JVM has reported on this request (FILT-10). See [`BreakpointInfo::hits`] for
    /// what the number counts and why it is not called `hit_count`.
    pub hits: u32,
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
    /// Per-value length cap for this stop point's captures (TRACE-9), or `None` for the defaults (100
    /// for a local, 200 for the `trace_expr` result). Kept beside `trace_frames` and for the same
    /// reason: a disable and re-arm must not quietly hand back a narrower capture than the one armed.
    pub trace_max_length: Option<usize>,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    /// Thread this request is filtered to (`ThreadOnly`), if any — for the `list_stop_points` line (FILT-1).
    pub thread_filter: Option<u64>,
    /// The one object this stop point is scoped to (`InstanceOnly`, FILT-9), if any.
    ///
    /// A **weak** reference, like every object id here (ADR-0022): the debuggee may collect it, at which
    /// point the filter silently stops matching and the stop point reads as "never fired" — which is the
    /// failure this whole codebase corrects for, so `list_stop_points` checks and says so.
    pub instance_filter: Option<u64>,
}

// Five bools, and each is an independent property of the JDWP request as the protocol defines it
// (armed / spent by the debuggee / static / traced / which touches) rather than a parameter bag that
// wants splitting up — the same reason `ExceptionRequestInfo` carries this allow.
/// An active field watchpoint: a `FIELD_ACCESS` or `FIELD_MODIFICATION` event request on one field.
/// Tracked so it shows in `list_stop_points` and is cleared by `clear_stop_point` / panic like a
/// normal breakpoint — `ClearAllBreakpoints` does not touch it.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct WatchpointInfo {
    /// The live JDWP event-request id, or `None` while disabled (BP-2).
    pub request_id: Option<i32>,
    /// Whether this watch is currently armed in the JVM.
    pub enabled: bool,
    /// Set when the **debuggee** removed this request rather than the caller (FILT-8). See
    /// [`BreakpointInfo::spent`].
    pub spent: bool,
    /// The `Count` modifier this watch was armed with, kept so a re-arm reproduces it (FILT-8).
    pub hit_count: Option<i32>,
    /// How many accesses or modifications the JVM has reported on this watch (FILT-10). See
    /// [`BreakpointInfo::hits`] for what the number counts and why it is not called `hit_count`.
    pub hits: u32,
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
    /// Per-value length cap for this stop point's captures (TRACE-9), or `None` for the defaults (100
    /// for a local, 200 for the `trace_expr` result). Kept beside `trace_frames` and for the same
    /// reason: a disable and re-arm must not quietly hand back a narrower capture than the one armed.
    pub trace_max_length: Option<usize>,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    /// Thread this watch is filtered to (`ThreadOnly`), if any — for the `list_stop_points` line (FILT-1).
    pub thread_filter: Option<u64>,
    /// The one object this stop point is scoped to (`InstanceOnly`, FILT-9), if any.
    ///
    /// A **weak** reference, like every object id here (ADR-0022): the debuggee may collect it, at which
    /// point the filter silently stops matching and the stop point reads as "never fired" — which is the
    /// failure this whole codebase corrects for, so `list_stop_points` checks and says so.
    pub instance_filter: Option<u64>,
}

/// An active method-exit request (METH-1): a `METHOD_EXIT` / `METHOD_EXIT_WITH_RETURN_VALUE` request
/// reporting what a method returned, keyed by its `mexit_` id.
///
/// Tracked like every other stop point so `list_stop_points` shows it and `clear_stop_point` / `panic` /
/// `toggle_stop_point` handle it. A stop point this tool can create but not clear would be a SAFE-class
/// bug — and this is the kind least survivable if left armed, since a suspending method exit on a hot
/// method freezes the VM faster than anything else here.
#[derive(Debug, Clone)]
// Five bools, each an independent property of the JDWP request as the protocol defines it (armed /
// spent by the debuggee / traced / return-value kind / suspending) rather than a parameter bag.
#[allow(clippy::struct_excessive_bools)]
pub struct MethodExitRequestInfo {
    /// The `mexit_` id reported to the caller.
    pub id: String,
    /// The live JDWP request id, or `None` while disabled (BP-2).
    pub request_id: Option<i32>,
    pub enabled: bool,
    /// Set when the **debuggee** removed this request rather than the caller (FILT-8). See
    /// [`BreakpointInfo::spent`].
    pub spent: bool,
    /// The `Count` modifier this request was armed with, kept so a re-arm reproduces it (FILT-8).
    ///
    /// Refused together with [`Self::method`], and that refusal is the whole reason this field is not
    /// simply plumbed through like the other two: JDWP applies `Count` to the **request**, which fires
    /// for every method of a matching class, while the method filter is applied here. `Count` 3 with a
    /// filter on `save` therefore means "the 3rd exit of *any* method of this class" — usually a
    /// different method, which this side then drops, leaving a stop point the JVM has already deleted
    /// and that reported nothing. See `arm_one_method_exit`.
    pub hit_count: Option<i32>,
    /// How many exits of the *asked-for* method the JVM has reported (FILT-10). See
    /// [`BreakpointInfo::hits`] for what the number counts and why it is not called `hit_count`.
    ///
    /// "Asked-for" is load-bearing here and nowhere else: JDWP has no method-name modifier, so this
    /// request receives every method of a matching class and [`method_name_matches`] drops the rest.
    /// The tally is charged *after* that filter — counting before it would report thousands of hits on a
    /// stop point that reported three, which is a worse answer than the missing one this replaced.
    ///
    /// [`method_name_matches`]: crate::handlers::method_name_matches
    pub hits: u32,
    /// Dotted class pattern the caller gave, kept so a disabled request can be re-armed.
    pub class_pattern: String,
    /// `ClassExclude` patterns this request was armed with (STEP-1), kept so a re-arm reproduces them —
    /// a re-arm that quietly dropped them would hand back a far noisier stop point under the same id.
    pub exclude_classes: Vec<String>,
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
    /// Per-value length cap for this stop point's captures (TRACE-9), or `None` for the defaults (100
    /// for a local, 200 for the `trace_expr` result). Kept beside `trace_frames` and for the same
    /// reason: a disable and re-arm must not quietly hand back a narrower capture than the one armed.
    pub trace_max_length: Option<usize>,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    pub thread_filter: Option<u64>,
    /// The one object this stop point is scoped to (`InstanceOnly`, FILT-9), if any.
    ///
    /// A **weak** reference, like every object id here (ADR-0022): the debuggee may collect it, at which
    /// point the filter silently stops matching and the stop point reads as "never fired" — which is the
    /// failure this whole codebase corrects for, so `list_stop_points` checks and says so.
    pub instance_filter: Option<u64>,
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
    pub trace_expr: Option<String>,
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
/// ordinary [`BreakpointInfo`] under its own `bp_…` id and behaves exactly like one armed by name. This
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
    pub trace_expr: Option<String>,
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

#[derive(Debug, Clone)]
pub struct BreakpointInfo {
    /// The live JDWP request ids — **one per armed bytecode location** — or empty when the breakpoint
    /// is disabled, in which case its definition is kept so it can be re-armed but no request is set in
    /// the JVM (BP-1).
    ///
    /// A set rather than a single id because one source line can map to several bytecode locations:
    /// `javac` inlines a `finally` body once per exit path, so the line is in the table twice and a
    /// breakpoint that armed only the first copy fired on normal completion and never on the throw
    /// (BP-4, #78). The stop point stays **one** thing to the caller — ADR-0005's one-id-per-stop-point
    /// rule is untouched — and this is the internal multiplicity underneath it.
    ///
    /// A `Vec` rather than a primary-plus-extras pair specifically because this field is *looked up by*
    /// (a hit arrives carrying a request id, and something has to find the stop point it belongs to).
    /// Any lookup that checked only a primary would silently miss hits on the exception-path copy, which
    /// is the bug being fixed wearing a different hat. Go through [`Self::owns_request`] rather than
    /// matching on an element.
    pub request_ids: Vec<i32>,
    pub class_pattern: String,
    pub line: u32,
    pub method: Option<String>,
    /// Whether the breakpoint is currently armed in the JVM. A disabled breakpoint stays listed (so
    /// its `condition`/`trace_expr` aren't lost) but has no JDWP request and never fires (BP-1).
    pub enabled: bool,
    /// Set when the **debuggee** removed this stop point's request rather than the caller (FILT-8).
    ///
    /// A stop point armed with the `hit_count` (`Count`) modifier fires **once**, on the Nth occurrence,
    /// and the JVM then deletes the request itself. Nothing tracked that, so such a stop point went on
    /// being listed as armed forever and `clear_stop_point` went on trying to clear a request that was
    /// gone — the exact shape `CONTEXT.md` § **Request id** warns about, since ids are allocated by the
    /// debuggee and recur.
    ///
    /// The bookkeeping is exact rather than heuristic, and this is why: `Count` means the JVM reports
    /// **only** the Nth occurrence, so the *first* event ever received for such a request **is** the
    /// Nth. Any hit on a stop point carrying `hit_count: Some(_)` therefore makes it spent, with no
    /// counting on this side and no window in which we could be wrong about it.
    ///
    /// Distinct from `enabled: false`, which is the BP-1 toggle — a state the **caller** chose and can
    /// undo. Both end with no live request, and both keep the definition so a re-arm reproduces it, but
    /// only one of them is something the caller did. `enabled` is set false alongside this so the
    /// existing re-arm path is reached unchanged; the listing distinguishes them.
    pub spent: bool,
    /// How many times the JVM has reported a hit on this stop point (FILT-10).
    ///
    /// Named `hits` and not `hit_count` on purpose. `hit_count` is the *requested* `Count` modifier
    /// ([`BreakpointArm::hit_count`], and the caller-facing `hit_count` argument), which asks the JVM to
    /// report only the Nth occurrence. This is the *observed* tally, and the two are opposite ends of
    /// the same word: one is an instruction, one is a measurement. They shared a name until FILT-10, and
    /// the collision is how this field stayed dead — constructed `0`, never incremented, so
    /// `list_stop_points`' `Hits:` line had never once printed.
    ///
    /// **Cumulative across a disable and re-arm**, following BP-1's rule that a toggled stop point keeps
    /// its definition: it is the same caller-facing stop point, so its tally is its lifetime tally.
    pub hits: u32,
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
    /// Per-value length cap for this stop point's captures (TRACE-9), or `None` for the defaults (100
    /// for a local, 200 for the `trace_expr` result). Kept beside `trace_frames` and for the same
    /// reason: a disable and re-arm must not quietly hand back a narrower capture than the one armed.
    pub trace_max_length: Option<usize>,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7).
    pub trace_cost: TraceCost,
    /// DISC-8: the stale-bytecode caveat found when this stop point was armed, if there was a proof.
    ///
    /// Stored as well as reported, because the two arming paths differ in what they *can* report. The
    /// immediate path returns a reply and says it there; the **deferred** path arms inside the event pump
    /// when the class finally loads, where there is no reply to append to — so without this the caller who
    /// most needs the warning (they armed against a class that was not loaded yet) is the one who never
    /// sees it. `debug.list_stop_points` renders it for both.
    pub drift: Option<String>,
    /// One rendered label per classloader this stop point is armed on, in the order the JVM listed
    /// them, when the class name resolved to more than one copy (BP-5, #79). Empty otherwise — which is
    /// almost always, and is what keeps an ordinary listing byte-identical.
    ///
    /// Rendered at arm time rather than at list time because naming a loader costs JDWP round trips
    /// (its `objectID`, then its own reference type, then that type's signature) and `list_stop_points`
    /// is the tool a caller reaches for while deciding whether a trace is hurting a shared instance.
    pub loaders: Vec<String>,
    /// Everything needed to re-arm this breakpoint at the same location after a `toggle_stop_point`
    /// disable (BP-1). Kept for every armed breakpoint so disable→enable round-trips exactly.
    pub arm: BreakpointArm,
}

impl BreakpointInfo {
    /// Whether `req` is one of this breakpoint's armed requests.
    ///
    /// The only supported way to ask. See [`Self::request_ids`] for why a lookup must not match on one
    /// element: a breakpoint on a `finally` line owns two requests, and the second is the one that
    /// fires when the code being debugged failed.
    pub fn owns_request(&self, req: i32) -> bool {
        self.request_ids.contains(&req)
    }

    /// Whether this breakpoint currently has any live request in the JVM.
    pub fn is_armed(&self) -> bool {
        !self.request_ids.is_empty()
    }
}

/// The resolved JDWP location and modifiers for a breakpoint, kept so a disabled breakpoint can be
/// re-armed at exactly the same place with the same behaviour (BP-1).
#[derive(Debug, Clone)]
pub struct BreakpointArm {
    pub class_id: u64,
    pub method_id: u64,
    pub bytecode_index: u64,
    /// Every *other* place this one stop point is armed. Empty for the ordinary breakpoint.
    ///
    /// Two different multiplicities land here, which is why it is a full location rather than a bare
    /// bytecode index:
    ///  - **one line, several bytecode copies in the same method** — `javac` inlines a `finally` body
    ///    once per exit path (BP-4, #78), so `class_id` and `method_id` repeat and only the index moves;
    ///  - **one class name, several loaded copies of it** — every classloader that has loaded the name
    ///    defines its own reference type (BP-5, #79), so `class_id` differs too.
    ///
    /// They are the same mechanism deliberately. Both are "one caller-facing stop point over N armed
    /// JDWP requests", and building the second as a parallel mechanism would have meant two disarm
    /// paths, two budget rules and two ways to be wrong.
    ///
    /// Deliberately a primary plus extras rather than the `Vec` [`BreakpointInfo::request_ids`] uses,
    /// and the asymmetry is the point: these are only ever *read together* at the arming site, so
    /// keeping the first out of the collection makes "there is at least one location" true by
    /// construction. Request ids are *searched*, where the same shape would invite a lookup that
    /// checks only the primary.
    pub extra_locations: Vec<ArmedLocation>,
    pub suspend_policy: jdwp_client::SuspendPolicy,
    pub hit_count: Option<i32>,
    pub thread_filter: Option<u64>,
    /// The one object this stop point is scoped to (`InstanceOnly`, FILT-9), if any.
    ///
    /// A **weak** reference, like every object id here (ADR-0022): the debuggee may collect it, at which
    /// point the filter silently stops matching and the stop point reads as "never fired" — which is the
    /// failure this whole codebase corrects for, so `list_stop_points` checks and says so.
    pub instance_filter: Option<u64>,
}

/// One concrete place a stop point is armed: a bytecode index in a method of a reference type.
///
/// Carries `class_id` as well as the index because the second copy of a stop point is not always in the
/// same class — see [`BreakpointArm::extra_locations`].
#[derive(Debug, Clone, Copy)]
pub struct ArmedLocation {
    pub class_id: u64,
    pub method_id: u64,
    pub bytecode_index: u64,
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
        class_roots: Vec<std::path::PathBuf>,
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
            thread_suspends: std::collections::BTreeMap::new(),
            watchdog_task: None,
            last_watchdog_note: None,
            last_watchdog_seq: None,
            disarmed_traced_requests: VecDeque::new(),
            rethrow_chains: HashMap::new(),
            trace_disarms: std::collections::BTreeMap::new(),
            trace_disarms_dropped: 0,
            read_only,
            source_roots,
            class_roots,
            redefinitions: std::collections::BTreeMap::new(),
            pending_breakpoints: Vec::new(),
            pattern_sets: HashMap::new(),
            launched: None,
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
