// One stop point, whichever kind it is (CLEAN-4, #187).
//
// `CONTEXT.md` opens its stop-point section with "**Stop point**: anything armed in the debuggee that
// reports when execution reaches it. The umbrella over all five kinds." This module is that umbrella as a
// type. Before it there were five records — `BreakpointInfo`, `ExceptionRequestInfo`, `WatchpointInfo`,
// `MethodExitRequestInfo`, `MonitorRequestInfo` — each separately redeclaring `enabled`, `spent` and
// `hits`, and everything downstream fanned out by five: five `disable_*` functions of exactly 18 lines
// differing in six one-token hunks, five `rearm_*` of 32 to 45 lines in the same relation, seven per-kind
// export passes, five per-kind loops in the listing renderer.
//
// **Spent is the state that most needed one home.** It is the one thing the debuggee produces without
// telling anyone (ADR-0026), so a stale belief about it is a wrong reply — and it was five beliefs.

use crate::handlers::DriftCheck;

/// Which of the five kinds a stop point is, using `CONTEXT.md`'s words for them.
///
/// Fieldless, and separate from [`ArmedOn`] which carries the per-kind payload, for one reason: a
/// listing has to **state** the order it groups kinds in rather than inherit it from how they happen to
/// be stored, and an order is declared over names, not over values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StopPointKind {
    /// A stop point at one source location — the only kind that can carry a condition… and the only kind
    /// that can be deferred, and the only kind that forms a wildcard family.
    Line,
    /// A stop point on a thrown exception of a given class and its subclasses.
    Exception,
    /// A stop point on reads or writes of one field.
    Watchpoint,
    /// A stop point on returns from a matching method.
    MethodExit,
    /// A stop point on lock contention: one of the four `MONITOR_*` events.
    Monitor,
}

impl StopPointKind {
    /// The order `debug.list_stop_points` and the stop-point set export group kinds in.
    ///
    /// **Declared here rather than derived from storage.** Before CLEAN-4 the grouping was a property of
    /// the five separate maps — the renderer ran five per-kind loops, so reordering the fields of
    /// `DebugSession` would have reordered a caller's listing with nothing failing. It is now a property
    /// the renderer states, which is a thing a test can assert. Ordering *within* a kind was and remains
    /// unspecified: the storage is a `HashMap` and always was.
    pub const LISTING_ORDER: [Self; 5] =
        [Self::Line, Self::Exception, Self::Watchpoint, Self::MethodExit, Self::Monitor];

    /// Where this kind sits in [`Self::LISTING_ORDER`] — the sort key `handlers::in_listing_order`
    /// groups a listing by.
    ///
    /// A `match` rather than a search of the array, and that is the point: **it is the trip-wire for a
    /// sixth kind.** Adding a variant stops this compiling, which is the one thing that will send whoever
    /// adds it looking for the other places a kind has to be named by hand — `LISTING_ORDER` itself, and
    /// the five `count_of_kind` calls behind `list_stop_points`' header line and `debug.panic`'s reply,
    /// neither of which can be made generic because both wordings name the kinds one by one.
    /// `the_listing_order_covers_every_kind_once` is the half the compiler cannot do: it catches a rank
    /// being given and the array left alone.
    #[must_use]
    pub const fn listing_rank(self) -> usize {
        match self {
            Self::Line => 0,
            Self::Exception => 1,
            Self::Watchpoint => 2,
            Self::MethodExit => 3,
            Self::Monitor => 4,
        }
    }

    /// What a reply calls this kind — the kind, not the id (BP-9, #159).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Line => "line breakpoint",
            Self::Exception => "exception stop point",
            Self::Watchpoint => "field watchpoint",
            Self::MethodExit => "method-exit stop point",
            Self::Monitor => "monitor stop point",
        }
    }

    /// The glyph an **armed** stop point of this kind gets in `debug.list_stop_points`.
    ///
    /// Only the armed one: the other two states are not the kind's to choose, which is what
    /// [`StopPoint::glyph`] enforces by never letting a caller pass them separately.
    #[must_use]
    pub const fn armed_glyph(self) -> &'static str {
        match self {
            Self::Line => "✓",
            Self::Exception => "⚡",
            Self::Watchpoint => "👁",
            Self::MethodExit => "↩",
            Self::Monitor => "🔒",
        }
    }

    /// Whether this kind can carry a server-side **condition** at all (ADR-0045).
    ///
    /// False for exactly one kind, and not because plumbing it was awkward. A condition is evaluated on
    /// the hit thread, and a thread suspended at a `monitorenter` is blocked on the very lock in the
    /// snapshot — an expression that invokes anything needing that monitor cannot complete, so the
    /// debugger would wedge the thread it is reporting on. `debug.set_monitor_stop` takes no `condition`
    /// argument either; `min_duration_ms` is this kind's filter, and it needs nothing from the debuggee.
    #[must_use]
    pub const fn takes_condition(self) -> bool {
        !matches!(self, Self::Monitor)
    }
}

/// What one stop point is armed **on** — the part of it the kind decides, and the only part that varies.
///
/// The shared state (id, `enabled`, **spent**, the hit tally, the condition, the trace settings, the
/// filters) lives on [`StopPoint`] and is declared once. Everything here is a location, a class, a field,
/// a method or a monitor event kind: what the arming call chose, and what a re-arm has to reproduce.
///
/// **The rule is where the payload lives, not how many times it is matched on.** #187 phrased it as "the
/// per-kind wire call is the only match", and that phrasing did not survive contact: *rendering* is per
/// kind by nature — five kinds describe themselves in five wordings, and no amount of moving fields
/// changes that. What the rule actually catches is a match that reaches for a payload field in order to
/// decide something **shared**, which is the shape that made `spent` five beliefs.
///
/// So the matches that legitimately exist, and what each is for:
///  - [`Self::wire_noun`], [`Self::describe`] and [`StopPoint::describe_for_rescue`] here — pure functions of
///    the payload, which is why they are methods on it rather than five-arm matches in a caller;
///  - `clear_stop_point_requests` — the per-kind JDWP `Clear`, the one #187 named;
///  - `clear_one_stop_point` — that call's per-kind reply, which carries a per-kind tail for two kinds;
///  - `render_stop_point_line` — the listing's kind grouping, which #187 asked for explicitly;
///  - `stop_points_on` and `stop_point_set::Builder::push_stop_point` — a class-scoped description and the
///    `debug.set_*` tool plus its locator arguments.
///
/// None of them decides shared state. A new one that does is the sign this doc used to claim to be about.
///
/// The accessors below are how a caller reaches one kind's payload without a match at all, and they are
/// the reason the list above is six and not twenty.
#[derive(Debug, Clone)]
pub enum ArmedOn {
    Line(LineBreakpoint),
    Exception(ExceptionBreakpoint),
    Watchpoint(Watchpoint),
    MethodExit(MethodExitRequest),
    Monitor(MonitorStop),
}

impl ArmedOn {
    /// What a failed `Clear` calls the JDWP request it could not remove.
    ///
    /// The protocol's noun rather than the glossary's: this appears only in a wire-level failure, where
    /// what a reader needs is the request type the packet named. The five wordings are the ones the five
    /// `disable_*` functions each carried.
    #[must_use]
    pub const fn wire_noun(&self) -> &'static str {
        match self {
            Self::Line(_) => "breakpoint request",
            Self::Exception(_) => "exception request",
            Self::Watchpoint(_) => "field watch",
            Self::MethodExit(_) => "method-exit request",
            Self::Monitor(_) => "monitor request",
        }
    }

    /// How `debug.toggle_stop_point` names this stop point in the sentence that reports what it did.
    ///
    /// **`describe_`, not `label_`, and that is a rule rather than a preference.** On this surface `label`
    /// already means *the word for a kind* — [`StopPointKind::label`], `WatchKind::label`,
    /// `MonitorKind::label` — and `CONTEXT.md` uses it in a third sense again for the **debugger-measured**
    /// provenance marker. These two name *one particular stop point*, which is a different job, and calling
    /// them `toggle_label`/`rescue_label` put both meanings inside one expression: `format!("monitor {}",
    /// mon.kind.label())` in a function called `toggle_label`. That is the `inherited` collision `CLAUDE.md`
    /// records, arriving by a third route. `describe_*` is this crate's verb for rendering reply prose and
    /// has 35 other users.
    ///
    /// Short, because that reply has already printed the id beside it. Its twin
    /// [`StopPoint::describe_for_rescue`] is the long form, and the two are separate rather than one function
    /// with a verbosity flag because they answer to different readers — see there.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Line(bp) => format!("{}:{}", bp.class_pattern, bp.line),
            Self::Exception(er) => format!("exception {}", er.class_pattern),
            Self::Watchpoint(wp) => format!("watch {}.{}", wp.class_name, wp.field_name),
            Self::MethodExit(me) => format!("method-exit {}", me.class_pattern),
            Self::Monitor(mon) => format!("monitor {}", mon.kind.label()),
        }
    }

    /// Which kind this is, with the payload stripped off.
    #[must_use]
    pub const fn kind(&self) -> StopPointKind {
        match self {
            Self::Line(_) => StopPointKind::Line,
            Self::Exception(_) => StopPointKind::Exception,
            Self::Watchpoint(_) => StopPointKind::Watchpoint,
            Self::MethodExit(_) => StopPointKind::MethodExit,
            Self::Monitor(_) => StopPointKind::Monitor,
        }
    }
}

/// Anything armed in the debuggee that reports when execution reaches it — all five kinds, one type.
///
/// Keyed in [`DebugSession::stop_points`](crate::session::DebugSession::stop_points) by its
/// [**stop-point id**](Self::id), which is the caller's handle on it and is not a JDWP **request id**
/// (ADR-0005).
// Three bools — `enabled`, `spent`, `trace` — and each is an independent property of the JDWP request as
// the protocol defines it (armed / spent by the debuggee / traced) rather than a parameter bag that wants
// splitting up. The five records this replaces each carried four or five and an
// `#[allow(clippy::struct_excessive_bools)]` to go with it; collapsing them dropped the duplicates and the
// allow with them.
#[derive(Debug, Clone)]
pub struct StopPoint {
    /// The caller-facing id — `bp_1`, `exc_2`, `watch_modify_3`, `mexit_4`, `mon_blocked_5`.
    ///
    /// Kept on the record as well as being the map key because most replies name it while holding the
    /// value rather than the entry, and because ADR-0005 makes the prefix part of what a caller reads.
    pub id: String,
    /// The live JDWP request ids — **one per armed bytecode location** — or empty when the stop point is
    /// disabled or **spent**, in which case its definition is kept so it can be re-armed but nothing is
    /// set in the JVM (BP-1).
    ///
    /// A `Vec` for every kind, though only a line breakpoint can hold more than one. One source line can
    /// map to several bytecode locations: `javac` inlines a `finally` body once per exit path, so the line
    /// is in the table twice and a breakpoint that armed only the first copy fired on normal completion
    /// and never on the throw (BP-4, #78). The stop point stays **one** thing to the caller — ADR-0005's
    /// one-id-per-stop-point rule is untouched — and this is the internal multiplicity underneath it.
    ///
    /// The other four kinds hold zero or one, which is what their `Option<i32>` used to say. A `Vec` of 0
    /// or 1 says the same thing and lets every lookup, disarm and clear be written once. Go through
    /// [`Self::owns_request`] rather than matching on an element: any lookup that checked only a first
    /// element would silently miss hits on a `finally` line's exception-path copy, which is the BP-4 bug
    /// wearing a different hat.
    pub request_ids: Vec<i32>,
    /// Whether this stop point is currently armed in the JVM. A disabled one stays listed (so its
    /// `condition`/`trace_expr` aren't lost) but has no JDWP request and never fires (BP-1).
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
    /// **only** the Nth occurrence, so the *first* event ever received for such a request **is** the Nth.
    /// Any hit on a stop point carrying `hit_count: Some(_)` therefore makes it spent, with no counting on
    /// this side and no window in which we could be wrong about it.
    ///
    /// Distinct from `enabled: false`, which is the BP-1 toggle — a state the **caller** chose and can
    /// undo. Both end with no live request, and both keep the definition so a re-arm reproduces it, but
    /// only one of them is something the caller did. `enabled` is set false alongside this so the existing
    /// re-arm path is reached unchanged; the listing distinguishes them.
    ///
    /// **Declared once, for all five kinds.** ADR-0026's two consequences — never list a spent stop point
    /// as armed, never send a clear for its **request id** — are the reason CLEAN-4 exists: a rule about a
    /// stale belief cannot be allowed to land on three kinds and miss two.
    pub spent: bool,
    /// The `Count` modifier this stop point was armed with, kept so a re-arm reproduces it (FILT-8).
    ///
    /// Named `hit_count` and not `hits`: this is the *requested* selector saying which single occurrence
    /// to stop on, which is the opposite kind of thing from the observed tally beside it. See
    /// [`Self::hits`].
    ///
    /// On a method-exit stop point it is **refused** together with a `method` filter, and that refusal is
    /// why the field is not simply plumbed through: JDWP applies `Count` to the **request**, which fires
    /// for every method of a matching class, while the method filter is applied on this side. `Count` 3
    /// with a filter on `save` therefore means "the 3rd exit of *any* method of this class" — usually a
    /// different method, which this side then drops, leaving a stop point the JVM has already deleted and
    /// that reported nothing. See `arm_one_method_exit`.
    pub hit_count: Option<i32>,
    /// How many times the JVM has reported a hit on this stop point (FILT-10).
    ///
    /// Named `hits` and not `hit_count` on purpose. [`Self::hit_count`] is the *requested* `Count`
    /// modifier (and the caller-facing `hit_count` argument), which asks the JVM to report only the Nth
    /// occurrence. This is the *observed* tally, and the two are opposite ends of the same word: one is an
    /// instruction, one is a measurement. They shared a name until FILT-10, and the collision is how this
    /// field stayed dead — constructed `0`, never incremented, so `list_stop_points`' `Hits:` line had
    /// never once printed.
    ///
    /// **Cumulative across a disable and re-arm**, following BP-1's rule that a toggled stop point keeps
    /// its definition: it is the same caller-facing stop point, so its tally is its lifetime tally.
    ///
    /// What it counts differs in exactly one place, and that place is documented where it happens:
    /// [`MethodExitRequest::discarded`] carries the exits this request was delivered and dropped by name,
    /// which are deliberately *not* counted here.
    pub hits: u32,
    /// Optional server-side condition (FILT-6, #83): on hit, evaluate it and let the hit go if it is not
    /// true. Kept on the record so a disable and re-arm reproduces it, exactly as `hit_count` is.
    ///
    /// **Always `None` on a monitor stop point**, and that is a rule rather than an accident of arming —
    /// see [`StopPointKind::takes_condition`], which is what refuses an edit that would set one.
    pub condition: Option<String>,
    /// Non-suspending trace mode: armed with `EventThread`, each hit is snapshotted into the trace ring
    /// buffer and the hit thread resumed, so a shared JVM is never frozen (TRACE-2).
    pub trace: bool,
    /// The trace expressions this stop point records, in the order given (TRACE-11, #93).
    /// Empty when it has none; one element is the pre-TRACE-11 case and renders identically.
    pub trace_expr: Vec<String>,
    /// Remaining trace-hit budget (TRACE-3): each traced hit decrements it, and on reaching zero the stop
    /// point disarms itself so a hot site can't flood the debuggee. `None` means unbounded.
    pub trace_budget: Option<u32>,
    /// How many caller frames each traced hit records above the hit frame (TRACE-5). 0 restores the
    /// original one-frame snapshot.
    pub trace_frames: usize,
    /// Per-value length cap for this stop point's captures (TRACE-9), or `None` for the defaults (100 for
    /// a local, 200 for the `trace_expr` result). Kept beside `trace_frames` and for the same reason: a
    /// disable and re-arm must not quietly hand back a narrower capture than the one armed.
    pub trace_max_length: Option<usize>,
    /// Observed capture cost, reported by `list_stop_points` (TRACE-7, ADR-0010).
    pub trace_cost: TraceCost,
    /// Thread this stop point is filtered to (`ThreadOnly`), if any — the cheap narrowing, applied inside
    /// the JVM (FILT-1).
    pub thread_filter: Option<u64>,
    /// The one object this stop point is scoped to (`InstanceOnly`, FILT-9), if any.
    ///
    /// A **weak** reference, like every object id here (ADR-0022): the debuggee may collect it, at which
    /// point the filter silently stops matching and the stop point reads as "never fired" — which is the
    /// failure this whole codebase corrects for, so `list_stop_points` checks and says so.
    ///
    /// Always `None` on a monitor stop point: it is refused at arm time (measured inert on that kind), so
    /// there is never one to report as dead.
    pub instance_filter: Option<u64>,
    /// What this stop point is armed on — the only part of it the kind decides.
    pub armed_on: ArmedOn,
}

impl StopPoint {
    /// Which of the five kinds this is.
    #[must_use]
    pub const fn kind(&self) -> StopPointKind {
        self.armed_on.kind()
    }

    /// Whether `req` is one of this stop point's armed requests.
    ///
    /// The only supported way to ask. See [`Self::request_ids`] for why a lookup must not match on one
    /// element: a breakpoint on a `finally` line owns two requests, and the second is the one that fires
    /// when the code being debugged failed.
    #[must_use]
    pub fn owns_request(&self, req: i32) -> bool {
        self.request_ids.contains(&req)
    }

    /// Whether this stop point currently has any live request in the JVM.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        !self.request_ids.is_empty()
    }

    /// The status glyph for this stop point, where a **spent** one is neither armed nor switched off by
    /// anyone.
    ///
    /// Takes the stop point rather than `(enabled, spent, armed_glyph)`, which is what it used to take:
    /// three loose values in an order a call site could get wrong, computed at five call sites that each
    /// had to remember which glyph belonged to their kind.
    ///
    /// **Spent is read first, and that ordering is the enforcement.** ADR-0026's first consequence is that
    /// a spent stop point must never be listed as armed. `enabled` and `spent` are two fields, so
    /// `enabled && spent` is *representable* even though nothing constructs it — every path that sets
    /// `spent` clears `enabled` in the same breath. Testing `enabled` first, as this did, made ADR-0026
    /// hold by that convention rather than by anything here; testing `spent` first makes the rule the
    /// type's, and the convention merely tidy. [`Self::state_suffix`] follows the same order for the same
    /// reason.
    #[must_use]
    pub const fn glyph(&self) -> &'static str {
        if self.spent {
            "⏹"
        } else if self.enabled {
            self.kind().armed_glyph()
        } else {
            "✗"
        }
    }

    /// How a **rescue note** names this stop point after the watchdog (SAFE-2) or a spent trace budget
    /// (TRACE-3) disarmed it.
    ///
    /// The long form of [`ArmedOn::describe`], and separate from it rather than the same function with
    /// a flag, because the two answer to different readers. A toggle reply has already printed the id and
    /// the caller just asked for the thing by name; a rescue note reaches a caller who **was not there**,
    /// on a line with nothing else on it, so it has to carry the id and say what kind of thing was turned
    /// off. Collapsing them would make one of the two wrong.
    #[must_use]
    pub fn describe_for_rescue(&self) -> String {
        match &self.armed_on {
            ArmedOn::Line(bp) => format!("breakpoint {} at {}:{}", self.id, bp.class_pattern, bp.line),
            ArmedOn::Exception(er) => format!("exception breakpoint {} ({})", self.id, er.class_pattern),
            ArmedOn::Watchpoint(wp) => {
                format!("watchpoint {} ({}.{})", self.id, wp.class_name, wp.field_name)
            }
            ArmedOn::MethodExit(me) => format!(
                "method-exit request {} ({}{})",
                self.id,
                me.class_pattern,
                me.method.as_ref().map_or_else(|| ".*".to_string(), |m| format!(".{m}"))
            ),
            ArmedOn::Monitor(mon) => format!("monitor request {} ({})", self.id, mon.kind.label()),
        }
    }

    /// The trailing state clause for this stop point in `list_stop_points` (FILT-8).
    ///
    /// Three states, not two, and the third is the point. `enabled: false` is BP-1's toggle — something
    /// the CALLER did and can undo. **Spent** is something the DEBUGGEE did: a stop point armed with
    /// `hit_count` fires once, on the Nth occurrence, and the JVM deletes the request itself. Both end
    /// with nothing armed and both keep the definition, so they render through one function; collapsing
    /// them into one WORDING would tell a caller their own toggle turned something off that they never
    /// touched.
    ///
    /// Before FILT-8 there was no third state at all: a spent stop point listed as armed, indefinitely,
    /// and `clear_stop_point` on it tried to clear a request the JVM had removed.
    #[must_use]
    pub const fn state_suffix(&self) -> &'static str {
        if !self.spent && self.enabled {
            ""
        } else if self.spent {
            " — SPENT (its hit_count fired, and the JVM deleted the request itself — nothing is armed. \
             debug.toggle_stop_point re-arms it with the same count)"
        } else {
            " — DISABLED (definition kept; toggle to re-arm)"
        }
    }

    /// The clause `clear_stop_point` appends when the stop point it just dropped was already **spent**
    /// (FILT-8) — its `hit_count` had fired and the JVM deleted the request itself.
    ///
    /// Worth a clause rather than silence because "✅ cleared" otherwise claims something that did not
    /// happen: no JDWP packet was sent, because there was no request left to name. The alternative most
    /// debuggers take — always send the `Clear` and ignore the error — is specifically wrong here.
    /// Request ids are allocated by the debuggee and **recur** (`CONTEXT.md` § **Request id**), so a
    /// `Clear` naming a long-deleted id can land on whatever now holds it. Not sending it is the
    /// correctness property; saying so is how the caller can tell it was not sent.
    #[must_use]
    pub const fn clear_note(&self) -> &'static str {
        if self.spent {
            " — it was already SPENT (its hit_count had fired and the JVM deleted the request), so \
             nothing was sent to the debuggee; only the bookkeeping is gone"
        } else {
            ""
        }
    }

    /// This stop point's line-breakpoint payload, or `None` if it is another kind.
    #[must_use]
    pub const fn line(&self) -> Option<&LineBreakpoint> {
        match &self.armed_on {
            ArmedOn::Line(l) => Some(l),
            _ => None,
        }
    }

    /// See [`Self::line`]; mutably, for the paths that arm a later copy or re-resolve a location.
    pub const fn line_mut(&mut self) -> Option<&mut LineBreakpoint> {
        match &mut self.armed_on {
            ArmedOn::Line(l) => Some(l),
            _ => None,
        }
    }

    /// This stop point's method-exit payload, or `None` if it is another kind.
    #[must_use]
    pub const fn method_exit(&self) -> Option<&MethodExitRequest> {
        match &self.armed_on {
            ArmedOn::MethodExit(m) => Some(m),
            _ => None,
        }
    }

    /// See [`Self::method_exit`]; mutably, for the discarded-exit tally (TRACE-15).
    pub const fn method_exit_mut(&mut self) -> Option<&mut MethodExitRequest> {
        match &mut self.armed_on {
            ArmedOn::MethodExit(m) => Some(m),
            _ => None,
        }
    }

    /// This stop point's monitor payload, or `None` if it is another kind.
    #[must_use]
    pub const fn monitor(&self) -> Option<&MonitorStop> {
        match &self.armed_on {
            ArmedOn::Monitor(m) => Some(m),
            _ => None,
        }
    }
}

/// A stop point at one source location — `CONTEXT.md`'s **line breakpoint**.
#[derive(Debug, Clone)]
pub struct LineBreakpoint {
    pub class_pattern: String,
    pub line: u32,
    pub method: Option<String>,
    /// The `line` the **caller** asked for, which is not always [`Self::line`] and is never `Some` when
    /// they asked by method name (BP-8, #135).
    ///
    /// Kept for the export, and the distinction is the whole reason it exists. [`Self::line`] and
    /// [`Self::method`] are what the *resolver* concluded — `method` is always `Some` once armed, filled in
    /// from the method the line landed in. Exporting those would write a resolution artefact into a field
    /// that means *request*: a stop point armed as `{line: 28}` would come back as
    /// `{line: 28, method: "classify"}`, which on a redeployed build where line 28 has moved into another
    /// method resolves differently or is refused outright. The same mistake as carrying an instance handle
    /// across a JVM, one level less obvious.
    ///
    /// So a set replays the caller's words. Both may be `Some` — a caller names both to disambiguate a
    /// line that appears in more than one method — and both may be `None` only for a family member
    /// re-pointed from a spec that had neither, which cannot arm.
    pub arm_line: Option<i32>,
    /// The `method` the **caller** asked for. See [`Self::arm_line`]; `None` when they armed by line alone.
    pub arm_method: Option<String>,
    /// DISC-8 and DISC-14: what the arming path found out about the build behind this stop point's line —
    /// a proof of drift, a proof of agreement, or the reason it could not tell.
    ///
    /// Stored as well as reported, because the two arming paths differ in what they *can* report. The
    /// immediate path returns a reply and says it there; the **deferred** path arms inside the event pump
    /// when the class finally loads, where there is no reply to append to — so without this the caller who
    /// most needs the warning (they armed against a class that was not loaded yet) is the one who never
    /// sees it. `debug.list_stop_points` renders it for both.
    ///
    /// A three-state verdict rather than `Option<String>` since DISC-14 (#130): "compared, and they agree"
    /// and "there was nothing to compare" were both `None`, which is the one distinction a silent reply
    /// cannot carry — see [`crate::handlers::DriftCheck`].
    pub drift: DriftCheck,
    /// One rendered label per classloader this stop point is armed on, in the order the JVM listed them,
    /// when the class name resolved to more than one copy (BP-5, #79). Empty otherwise — which is almost
    /// always, and is what keeps an ordinary listing byte-identical.
    ///
    /// Rendered at arm time rather than at list time because naming a loader costs JDWP round trips (its
    /// `objectID`, then its own reference type, then that type's signature) and `list_stop_points` is the
    /// tool a caller reaches for while deciding whether a trace is hurting a shared instance.
    pub loaders: Vec<String>,
    /// Everything needed to re-arm this breakpoint at the same location after a `toggle_stop_point`
    /// disable (BP-1). Kept for every armed breakpoint so disable→enable round-trips exactly.
    pub arm: BreakpointArm,
    /// Whether copies of this class loaded LATER will be armed, and by what (BP-7, #115).
    pub rearm: RearmState,
}

/// A stop point on a thrown exception of a given class and its subclasses — `CONTEXT.md`'s **exception
/// breakpoint**.
#[derive(Debug, Clone)]
pub struct ExceptionBreakpoint {
    // There was a `ref_type: Option<u64>` here, described as "kept so a disabled request can be re-armed
    // (BP-2)". Nothing ever read it, and BP-4 is why: a re-arm re-resolves the class **by name**, because
    // a reference type id is only valid while that type stays loaded and the realistic sequence is
    // "disable, redeploy, re-arm". The field was written on every arm and every re-arm and read nowhere —
    // write-only since BP-4 superseded the reason it existed. Five near-identical records is how it stayed
    // invisible; one made the compiler say so (CLEAN-4). `Watchpoint` carried the same field for the same
    // reason and lost it in the same commit.
    /// Dotted class pattern the caller gave, or "*" for all exceptions.
    pub class_pattern: String,
    pub caught: bool,
    pub uncaught: bool,
}

/// A stop point on reads or writes of one field — `CONTEXT.md`'s **watchpoint**.
#[derive(Debug, Clone)]
pub struct Watchpoint {
    // There was an `arm: (u64, u64)` here — the declaring type and field id, described as kept "only so a
    // disabled watch can be re-armed (BP-2)". It never was: a re-arm re-resolves both **by name** (BP-4),
    // and reporting a hit deliberately does not use them either, since a hit carries its own declaring
    // type and field so `get_last_event` can describe a hit whose watchpoint has already been cleared.
    // Write-only, like `ExceptionBreakpoint`'s `ref_type` and for the same reason.
    /// Which event kind this was registered as — `Clear` needs the same kind back.
    pub kind: jdwp_client::WatchKind,
    /// Dotted class name the caller gave, for messages.
    pub class_name: String,
    pub field_name: String,
    /// Whether the field is static, for the `list_stop_points` line.
    pub is_static: bool,
}

/// A stop point on returns from a matching method — `CONTEXT.md`'s **method-exit request** (METH-1).
#[derive(Debug, Clone)]
pub struct MethodExitRequest {
    /// How many exits of *other* methods of a matching class arrived and were dropped by
    /// `method_name_matches` (TRACE-15, #156). Meaningless without [`Self::method`], since a request with
    /// no method filter drops nothing by name.
    ///
    /// This is the complement of [`StopPoint::hits`] rather than a second version of it, and the pair is
    /// what makes `Hits: 0` a diagnosis instead of a question. Alone it cannot distinguish *the code never
    /// ran* from *it fired constantly and every event was discarded*, which is the case that cost the
    /// reporter two end-to-end runs and nearly a supplier bug report: a request that went from 3.2 s
    /// unarmed to a 240 s read timeout armed still read `Hits: 0`, because every exit delivered belonged
    /// to another method. Beside `discarded: 0` that same line means the class produced no exits at all.
    ///
    /// It also makes the arming cost visible, which nothing else here reports. Every discarded exit is a
    /// packet the debuggee generated, notified and sent over the one connection this server multiplexes —
    /// `.out-of-scope/method-entry-events.md` is the recorded reasoning about that volume, and this is the
    /// same cost measured rather than argued.
    ///
    /// Charged at the drop sites in [`crate::handlers`], which have already called `method_name_matches`
    /// either way, so the count needs no extra JDWP round trip.
    pub discarded: u32,
    /// Dotted class pattern the caller gave, kept so a disabled request can be re-armed.
    pub class_pattern: String,
    /// `ClassExclude` patterns this request was armed with (STEP-1), kept so a re-arm reproduces them — a
    /// re-arm that quietly dropped them would hand back a far noisier stop point under the same id.
    pub exclude_classes: Vec<String>,
    /// Method name to report on, filtered on OUR side: JDWP has no method-name modifier, so the request
    /// fires for every method of a matching class and non-matching exits are dropped by the event pump.
    /// `None` means every method — only allowed in trace mode.
    pub method: Option<String>,
    /// Whether this was armed as `METHOD_EXIT_WITH_RETURN_VALUE` (kind 42). Needed to clear it, since JDWP
    /// keys requests by (eventKind, requestID); also says whether a hit can report a value at all.
    pub with_return_value: bool,
}

/// A stop point on lock contention — `CONTEXT.md`'s **monitor stop point** (DUMP-7, #96).
///
/// The fifth kind, and the only one whose site the caller does **not** choose: contention happens wherever
/// threads collide.
#[derive(Debug, Clone)]
pub struct MonitorStop {
    /// Which of the four events this request is armed for. Needed to clear it — JDWP keys requests by
    /// (eventKind, requestID) and the four kinds are four separate keys, so clearing with the wrong one
    /// silently leaves a possibly-suspending stop point armed with nothing on this side able to find it.
    pub kind: jdwp_client::MonitorKind,
    /// Whether this call also armed the other half of this kind's pair.
    ///
    /// Recorded because it decides what a snapshot can honestly claim: a duration is measured **across**
    /// the two halves, so an unpaired request can only report that the event happened. Kept on the record
    /// rather than re-derived by scanning the map, so the answer cannot change under a caller who clears
    /// the partner — at which point the remaining half must start saying it can no longer measure.
    pub paired: bool,
    /// The type a `ClassOnly` modifier was armed with, kept so a re-arm reproduces it — and stored as the
    /// dotted NAME as well, because re-arming re-resolves by name (BP-4): a reference type id is only
    /// valid while that type stays loaded.
    pub monitor_class: Option<String>,
    /// Only report a resolved pair whose measured duration is at least this many milliseconds (DUMP-7's
    /// `min_duration_ms`).
    ///
    /// **A filter on what you READ, not on what crosses the wire**, and the distinction is not pedantry:
    /// JDWP has no duration modifier, so the event has already been generated, has already cost the
    /// debuggee its notification, and has already arrived here before this can be applied. It reduces
    /// noise in the trace buffer and nothing else. `None` records every pair.
    pub min_duration_ms: Option<u64>,
}

/// What is watching for later copies of an armed stop point's class (BP-7, #115).
///
/// Three states rather than an `Option`, because two of the three "no standing watch of my own" cases mean
/// opposite things to a caller and the listing has to say so. A wildcard family's member is covered by the
/// FAMILY's watch — a redeploy's copy matches the pattern and is armed as a new member, under its own
/// `bp_` id — so telling its owner to re-arm after a redeploy would be false. A stop point whose watch
/// could not be registered genuinely does need re-arming, and that has to be legible rather than inferred
/// from an absent line.
#[derive(Debug, Clone)]
pub enum RearmState {
    /// Holding its own watch, and will arm later copies into this same stop point.
    Watching(ReArmWatch),
    /// A wildcard family's member (FILT-3). The family owns one watch between all of them.
    CoveredByFamily,
    /// Nothing is watching. Said out loud, because it behaves exactly as everything did before #115.
    Unwatched,
}

impl RearmState {
    /// The watch, when this stop point owns one.
    #[must_use]
    pub const fn watch(&self) -> Option<&ReArmWatch> {
        match self {
            Self::Watching(w) => Some(w),
            _ => None,
        }
    }

    /// The watch, mutably — for the counter that makes a redeploy legible in `list_stop_points`.
    pub const fn watch_mut(&mut self) -> Option<&mut ReArmWatch> {
        match self {
            Self::Watching(w) => Some(w),
            _ => None,
        }
    }
}

/// The `CLASS_PREPARE` watch an **armed** exact-name stop point keeps for the rest of its life (BP-7, #115).
///
/// Before this, an exact name watched for its class exactly once, ever: a deferred breakpoint cleared its
/// watch the moment it armed, and a stop point that armed immediately never registered one. A wildcard
/// family kept arming classes that loaded later by design; an exact name did not — and **a redeploy is
/// precisely "this class loads again"**. The new deployment gets a new module classloader, nothing arms the
/// copy in it, and the stop point stays enabled, stays listed, and watches the retired deployment's copy.
///
/// What makes that worth a whole mechanism rather than a caveat is that the failure is **silent and shaped
/// exactly like being wrong**: Rule 0 puts you on `trace:true`, which by design produces nothing when it
/// does not fire, so an empty `get_traces` reads as "the code path I predicted is not the one running" and
/// you go back to re-read the code. Its loud twin — a member lookup against the same retired copy — is
/// EVAL-13 (#116). See `CONTEXT.md` § **Copy**.
#[derive(Debug, Clone)]
pub struct ReArmWatch {
    /// The live `CLASS_PREPARE` request id. Cleared when the stop point is cleared, never when it arms.
    pub request_id: i32,
    /// The JNI signature this watch matches, compared against the `ClassPrepare` event's own.
    pub signature: String,
    /// How many copies this watch has armed SINCE the stop point was set. Counted because it is the one
    /// number that distinguishes "armed on 4 classloaders because this library is in four wars" from
    /// "armed on 4 because you have redeployed three times", and `list_stop_points` is where a reader goes
    /// to tell those apart.
    pub later_copies: usize,
    /// The location as the CALLER asked for it, not as it resolved. A later copy is re-resolved from
    /// these, and it has to be: a stop point armed by method name whose resolved line was then reused as
    /// the target would land wherever that line number happens to be in the redeployed class, which is
    /// exactly the drift the caller never asked for.
    pub line: Option<i32>,
    pub method: Option<String>,
}

/// The resolved JDWP location and suspend policy for a breakpoint, kept so a disabled breakpoint can be
/// re-armed at exactly the same place with the same behaviour (BP-1).
///
/// The `hit_count`, `thread_filter` and `instance_filter` that used to live here now live on
/// [`StopPoint`] with every other kind's copies of them (CLEAN-4). They were never location state: keeping
/// them here is what made `apply_stop_point_edit` write a line breakpoint's count to a different place
/// from the other four kinds', with a comment explaining the exception.
#[derive(Debug, Clone)]
pub struct BreakpointArm {
    pub class_id: u64,
    pub method_id: u64,
    pub bytecode_index: u64,
    /// Every *other* place this one stop point is armed. Empty for the ordinary breakpoint.
    ///
    /// Two different multiplicities land here, which is why it is a full location rather than a bare
    /// bytecode index:
    ///  - **one line, several bytecode copies in the same method** — `javac` inlines a `finally` body once
    ///    per exit path (BP-4, #78), so `class_id` and `method_id` repeat and only the index moves;
    ///  - **one class name, several loaded copies of it** — every classloader that has loaded the name
    ///    defines its own reference type (BP-5, #79), so `class_id` differs too.
    ///
    /// They are the same mechanism deliberately. Both are "one caller-facing stop point over N armed JDWP
    /// requests", and building the second as a parallel mechanism would have meant two disarm paths, two
    /// budget rules and two ways to be wrong.
    ///
    /// Deliberately a primary plus extras rather than the `Vec` [`StopPoint::request_ids`] uses, and the
    /// asymmetry is the point: these are only ever *read together* at the arming site, so keeping the
    /// first out of the collection makes "there is at least one location" true by construction. Request
    /// ids are *searched*, where the same shape would invite a lookup that checks only the primary.
    pub extra_locations: Vec<ArmedLocation>,
    pub suspend_policy: jdwp_client::SuspendPolicy,
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

/// What a traced stop point has actually cost, measured hit by hit (TRACE-7, ADR-0010).
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn capture_share(&self) -> Option<f64> {
        Some(self.observed_rate()? * self.mean_capture()?.as_secs_f64())
    }
}

/// Stop points assembled in memory, for the tests that need one and no JVM.
///
/// The same move `classfile::build` makes one layer down, and named after it. It is not a recording of a
/// session and it is not a double standing in for one — it is a stop point in a stated state, which is the
/// only thing a rule about `enabled`/**spent**/`hits` needs in order to be checked. See `CONTEXT.md` §
/// **Stated**.
#[cfg(test)]
pub mod build {
    use super::{ArmedOn, DriftCheck, StopPoint, StopPointKind, TraceCost};

    /// A stop point of `kind`, armed on one live request, with everything optional left off.
    pub fn armed(id: &str, kind: StopPointKind) -> StopPoint {
        StopPoint {
            id: id.to_string(),
            request_ids: vec![7],
            enabled: true,
            spent: false,
            hit_count: None,
            hits: 0,
            condition: None,
            trace: false,
            trace_expr: Vec::new(),
            trace_budget: None,
            trace_frames: 0,
            trace_max_length: None,
            trace_cost: TraceCost::default(),
            thread_filter: None,
            instance_filter: None,
            armed_on: armed_on(kind),
        }
    }

    /// The quietest payload each kind can have.
    fn armed_on(kind: StopPointKind) -> ArmedOn {
        match kind {
            StopPointKind::Line => ArmedOn::Line(super::LineBreakpoint {
                class_pattern: "com.example.Orders".to_string(),
                line: 42,
                method: Some("save".to_string()),
                arm_line: Some(42),
                arm_method: None,
                drift: DriftCheck::NotChecked("a stated stop point never reached a JVM".to_string()),
                loaders: Vec::new(),
                arm: super::BreakpointArm {
                    class_id: 0x10,
                    method_id: 0x20,
                    bytecode_index: 0,
                    extra_locations: Vec::new(),
                    suspend_policy: jdwp_client::SuspendPolicy::EventThread,
                },
                rearm: super::RearmState::Unwatched,
            }),
            StopPointKind::Exception => ArmedOn::Exception(super::ExceptionBreakpoint {
                class_pattern: "java.lang.IllegalStateException".to_string(),
                caught: true,
                uncaught: true,
            }),
            StopPointKind::Watchpoint => ArmedOn::Watchpoint(super::Watchpoint {
                kind: jdwp_client::WatchKind::Modify,
                class_name: "com.example.Orders".to_string(),
                field_name: "total".to_string(),
                is_static: false,
            }),
            StopPointKind::MethodExit => ArmedOn::MethodExit(super::MethodExitRequest {
                discarded: 0,
                class_pattern: "com.example.Orders".to_string(),
                exclude_classes: Vec::new(),
                method: Some("save".to_string()),
                with_return_value: true,
            }),
            StopPointKind::Monitor => ArmedOn::Monitor(super::MonitorStop {
                kind: jdwp_client::MonitorKind::Blocked,
                paired: true,
                monitor_class: None,
                min_duration_ms: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build, StopPoint, StopPointKind};

    /// Where each kind belongs in a listing, written as an exhaustive `match` so that adding a sixth
    /// variant stops this test compiling. That is the forcing function: a new kind cannot be added
    /// without somebody deciding where it goes, and the length assertion below is what catches deciding
    /// and then leaving [`StopPointKind::LISTING_ORDER`] alone.
    const fn declared_rank(kind: StopPointKind) -> usize {
        match kind {
            StopPointKind::Line => 0,
            StopPointKind::Exception => 1,
            StopPointKind::Watchpoint => 2,
            StopPointKind::MethodExit => 3,
            StopPointKind::Monitor => 4,
        }
    }

    /// The order a listing groups kinds in is something the renderer STATES now, not something it
    /// inherits from how stop points are stored — before CLEAN-4 it was a property of five separate
    /// `DebugSession` fields, so reordering those fields would have reordered a caller's listing with
    /// nothing failing.
    ///
    /// A kind absent from `LISTING_ORDER` compiles fine and is a stop point `debug.list_stop_points`
    /// never prints — silence of exactly the shape this tool exists to remove.
    #[test]
    fn the_listing_order_covers_every_kind_once() {
        assert_eq!(
            StopPointKind::LISTING_ORDER.len(),
            5,
            "a kind added to the enum needs a place in LISTING_ORDER too, or it is never listed"
        );
        for kind in StopPointKind::LISTING_ORDER {
            assert_eq!(
                StopPointKind::LISTING_ORDER.iter().position(|k| *k == kind),
                Some(declared_rank(kind)),
                "{kind:?} is not where it says it is"
            );
        }
    }

    /// FILT-8 / ADR-0026: the three states are three, on **every** kind. Before CLEAN-4 these rules lived
    /// in two functions taking three loose booleans and were reached from five call sites that each
    /// supplied their own glyph; the cross-product below could not be written, because there was no one
    /// value to write it over.
    #[test]
    fn the_state_matrix_reads_the_same_on_every_kind() {
        for kind in StopPointKind::LISTING_ORDER {
            let armed = build::armed("id_1", kind);
            assert_eq!(armed.glyph(), kind.armed_glyph(), "{kind:?} armed");
            assert_eq!(armed.state_suffix(), "", "{kind:?}: an armed stop point says nothing extra");
            assert_eq!(armed.clear_note(), "", "{kind:?}: nothing to add when it was not spent");
            assert!(armed.is_armed(), "{kind:?}");

            // The caller's own toggle (BP-1).
            let mut disabled = build::armed("id_1", kind);
            disabled.enabled = false;
            disabled.request_ids.clear();
            assert_eq!(disabled.glyph(), "✗", "{kind:?} disabled");
            assert!(disabled.state_suffix().contains("DISABLED"), "{kind:?}");
            assert!(!disabled.state_suffix().contains("SPENT"), "{kind:?}");
            assert_eq!(disabled.clear_note(), "", "{kind:?}: a disable sends a clear like any other");

            // The DEBUGGEE's doing, which is a different fact and must not read as the caller's.
            let mut spent = build::armed("id_1", kind);
            spent.enabled = false;
            spent.spent = true;
            spent.request_ids.clear();
            assert_eq!(spent.glyph(), "⏹", "{kind:?}: spent has its own glyph");
            let suffix = spent.state_suffix();
            assert!(suffix.contains("SPENT"), "{kind:?}: {suffix}");
            assert!(
                !suffix.contains("DISABLED"),
                "{kind:?}: spent must not read as the caller's own toggle — they did not switch this \
                 off:\n{suffix}"
            );
            assert!(
                suffix.contains("toggle_stop_point"),
                "{kind:?}: a caller told their stop point is gone needs the way back:\n{suffix}"
            );
            assert!(
                spent.clear_note().contains("nothing was sent to the debuggee"),
                "{kind:?}: ADR-0026 — a clear that sent no packet has to say so:\n{}",
                spent.clear_note()
            );
        }
    }

    /// ADR-0026's first consequence — "such a stop point must not be listed as armed" — held only by
    /// convention until CLEAN-4's review: `enabled` and `spent` are two `pub` fields, so `enabled && spent`
    /// is representable, and the renderers tested `enabled` first. Nothing constructs that state (every
    /// path that spends a stop point clears `enabled` in the same breath), which is exactly why nothing
    /// would have caught the day something did.
    #[test]
    fn a_spent_stop_point_never_reads_as_armed_even_if_enabled_says_otherwise() {
        for kind in StopPointKind::LISTING_ORDER {
            let mut contradictory = build::armed("id_1", kind);
            contradictory.spent = true; // and `enabled` deliberately left true
            assert_eq!(
                contradictory.glyph(),
                "⏹",
                "{kind:?}: spent wins, or a listing claims the JVM still holds a request it deleted"
            );
            assert!(
                contradictory.state_suffix().contains("SPENT"),
                "{kind:?}: {}",
                contradictory.state_suffix()
            );
        }
    }

    /// The two descriptions are the same five kinds in two voices, and neither may borrow the other's: a
    /// toggle reply prints the id beside its own, a rescue note has nothing else on the line.
    #[test]
    fn a_rescue_description_names_the_id_and_a_toggle_description_does_not() {
        for kind in StopPointKind::LISTING_ORDER {
            let sp = build::armed("id_1", kind);
            assert!(
                sp.describe_for_rescue().contains("id_1"),
                "{kind:?}: a caller who was away is told only this:\n{}",
                sp.describe_for_rescue()
            );
            assert!(
                !sp.armed_on.describe().contains("id_1"),
                "{kind:?}: the toggle reply has already printed the id:\n{}",
                sp.armed_on.describe()
            );
        }
    }

    /// ADR-0045: the one kind that takes no condition, stated on the kind rather than remembered at each
    /// of the two places that used to check it.
    #[test]
    fn only_the_monitor_kind_refuses_a_condition() {
        for kind in StopPointKind::LISTING_ORDER {
            assert_eq!(
                kind.takes_condition(),
                kind != StopPointKind::Monitor,
                "{kind:?}: a condition is evaluated on the hit thread, and a thread suspended at a \
                 monitorenter is blocked on the very lock in the snapshot"
            );
        }
    }

    /// BP-4 (#78): one caller-facing stop point over several armed JDWP requests, and a lookup that
    /// matched only the first would miss hits on the exception-path copy — which is the bug wearing a
    /// different hat. The `Vec` is now every kind's, so the rule is checked once.
    #[test]
    fn a_lookup_finds_every_request_a_stop_point_owns() {
        let mut sp = build::armed("bp_1", StopPointKind::Line);
        sp.request_ids = vec![11, 12];
        assert!(sp.owns_request(11), "the primary");
        assert!(sp.owns_request(12), "the `finally` body's second copy, which fires on the throw");
        assert!(!sp.owns_request(13));

        // Disarmed, so a request id the debuggee has since reissued cannot match it (CONTEXT.md §
        // Request id).
        sp.request_ids.clear();
        assert!(!sp.owns_request(11));
        assert!(!sp.is_armed());
    }

    /// The payload accessors answer for their own kind and nothing else, which is what lets the rest of
    /// the crate reach one kind's fields without a `match` of its own.
    #[test]
    fn a_payload_accessor_answers_only_for_its_own_kind() {
        for kind in StopPointKind::LISTING_ORDER {
            let sp = build::armed("id_1", kind);
            assert_eq!(sp.kind(), kind);
            assert_eq!(sp.line().is_some(), kind == StopPointKind::Line, "{kind:?}");
            assert_eq!(sp.method_exit().is_some(), kind == StopPointKind::MethodExit, "{kind:?}");
            assert_eq!(sp.monitor().is_some(), kind == StopPointKind::Monitor, "{kind:?}");
        }
    }

    /// Every kind names itself, and no two kinds name themselves the same way — `debug.update_stop_point`
    /// prints this and a refusal that said "monitor stop point" about a watchpoint would send a caller to
    /// the wrong tool.
    #[test]
    fn every_kind_has_its_own_label_and_glyph() {
        let mut labels: Vec<&str> = StopPointKind::LISTING_ORDER.iter().map(|k| k.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), StopPointKind::LISTING_ORDER.len(), "two kinds share a label: {labels:?}");

        let mut glyphs: Vec<&str> = StopPointKind::LISTING_ORDER.iter().map(|k| k.armed_glyph()).collect();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), StopPointKind::LISTING_ORDER.len(), "two kinds share a glyph: {glyphs:?}");
        for k in StopPointKind::LISTING_ORDER {
            for taken in ["⏹", "✗"] {
                assert_ne!(
                    k.armed_glyph(),
                    taken,
                    "{k:?}'s armed glyph collides with a NOT-armed one, so a listing cannot be read"
                );
            }
        }
    }

    /// The wire noun is what a failed `Clear` names, and JDWP keys requests by (eventKind, requestID) —
    /// so a caller reading the failure has to be able to tell which request type did not go.
    #[test]
    fn every_kind_names_its_own_wire_request() {
        let nouns: Vec<&str> = StopPointKind::LISTING_ORDER
            .iter()
            .map(|k| build::armed("id_1", *k).armed_on.wire_noun())
            .collect();
        let mut unique = nouns.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), nouns.len(), "two kinds report the same failed request: {nouns:?}");
    }

    /// TRACE-7 / ADR-0010: a re-armed stop point starts its cost observation from scratch, and one
    /// capture establishes a cost but no interval.
    #[test]
    fn a_trace_cost_reports_a_rate_only_once_it_has_an_interval() {
        let sp: StopPoint = build::armed("bp_1", StopPointKind::Line);
        assert!(sp.trace_cost.mean_capture().is_none(), "unmeasured is not free");
        assert!(sp.trace_cost.observed_rate().is_none());

        let mut cost = sp.trace_cost;
        let start = std::time::Instant::now();
        cost.record(start, std::time::Duration::from_millis(2));
        assert_eq!(cost.mean_capture(), Some(std::time::Duration::from_millis(2)));
        assert!(cost.observed_rate().is_none(), "one capture spans no interval, so there is no rate yet");
    }
}
