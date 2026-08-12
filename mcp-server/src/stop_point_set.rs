//! A stop-point set: the armed stop points of a session, in a form that outlives the process (BP-8, #135).
//!
//! # Why the server does not write a file
//!
//! `debug.list_stop_points {export: true}` returns the set as content and `debug.arm_stop_points {set: …}`
//! takes it back. Nothing here touches the filesystem, and a file under the project and a dotfile in `$HOME`
//! were both rejected for it.
//!
//! The reason is the safety model rather than tidiness. Everything this server promises about a shared JVM
//! leans on process death being an **unambiguous end of session** — the watchdog, the resume accounting, the
//! read-only enforcement. State that outlives the process on disk is state no live process can vouch for, and
//! it would arrive with a policy about where output lands that this project has so far not needed to have.
//! The client already persists things; it does not need us to.
//!
//! # The format is a list of the calls that would recreate the set
//!
//! An entry is `{tool, enabled, args}` — literally the `debug.set_*` call and the arguments it would take.
//! This is not a coincidence of encoding, it is the design: [`crate::handlers`] re-arms a set by dispatching
//! each entry through the **same handler a caller would reach**, so every refusal, clamp, capability check,
//! deferral and read-only rule applies on the way back in without being reimplemented. A parallel arming path
//! would be a second place for those rules to live and a second place for them to drift.
//!
//! It also means the format is checked by something: the argument schemas are snapshot-tested
//! (`tests/argument-schemas.txt`), so an argument that is renamed or dropped cannot silently invalidate every
//! set anyone has saved.
//!
//! **`arm_stop_points` is not a generic batch executor**, and [`ARMABLE_TOOLS`] is why. Only the five
//! `set_*` tools can appear in a set. Without that list the tool would be a way to invoke anything in this
//! server from a blob of JSON, which is a different and much larger thing than resuming an investigation.
//!
//! # What cannot cross a JVM boundary, and is therefore dropped rather than restored
//!
//! **Instance filters** hold `@0x…` object handles, which are weak references (ADR-0022) meaningful only to
//! the JVM that issued them. **Thread filters** hold JDWP thread ids and are exactly the same problem: #135's
//! body named only the first, but `list_stop_points` already reports the two as separate warnings (FILT-2 and
//! FILT-9) precisely because the cause and the fix differ, and an export that carried either into a new JVM
//! would be re-arming a filter against whatever now happens to live at that address.
//!
//! Dropped, and **named in the reply**. A filter silently omitted turns a narrow stop point into a broad one,
//! which on a shared instance is the difference between a diagnostic and an outage.
//!
//! Resolved JDWP ids (`class_id`, `method_id`, `bytecode_index`, field ids) are not in the format at all: the
//! set carries the *caller's* description — class name, line, field name — and re-resolves it on the way back,
//! which is what makes a set usable against a redeployed build rather than only against the same process.

use crate::stop_point::{ArmedOn, StopPoint, StopPointKind};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde_json::{json, Map, Value};

/// The key that identifies this format, and the version beside it.
///
/// Distinctive on purpose: the failure to design for is a caller passing back something that is *not* a set —
/// a `list_stop_points` reply, a fragment of one, an object from another tool — and a bare `{"entries": […]}`
/// would leave nothing to recognise. See [`parse`] for what the refusals say.
pub const FORMAT_KEY: &str = "jdwp_mcp_stop_point_set";

/// Bumped only for a change that a version 1 reader would get *wrong*, not for one it would merely not use.
pub const FORMAT_VERSION: u64 = 1;

/// The only tools a set may name.
///
/// The whitelist that keeps `debug.arm_stop_points` from being a generic RPC batch. Every entry arms a stop
/// point and nothing else: nothing here steps, resumes, evaluates, sets a value or disconnects.
pub const ARMABLE_TOOLS: [&str; 5] = [
    "debug.set_line_stop",
    "debug.set_exception_stop",
    "debug.set_field_stop",
    "debug.set_method_exit_stop",
    "debug.set_monitor_stop",
];

/// One entry: the call that would arm one stop point.
#[derive(Debug, Clone)]
pub struct SetEntry {
    /// One of [`ARMABLE_TOOLS`].
    pub tool: String,
    /// Whether the stop point was armed **and enabled** when the set was exported.
    ///
    /// A `false` here is exported and then **skipped** on the way back in — see
    /// [`ArmOutcome::SkippedDisabled`] for why arming it is not the safe reading of the caller's intent.
    pub enabled: bool,
    /// The arguments, exactly as the tool named in [`Self::tool`] would take them.
    pub args: Value,
}

/// What an export could not carry, so the reply can say so instead of quietly narrowing nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// Stop-point ids whose `instance_id` was dropped (ADR-0022).
    pub instance: Vec<String>,
    /// Stop-point ids whose `thread_id` was dropped.
    pub thread: Vec<String>,
    /// Stop points that could not be described as a call at all, with the reason.
    ///
    /// One case reaches this today: a wildcard family whose members are all gone and which names no method,
    /// so there is no line and no method left to re-arm from. Reported rather than emitted as a half entry.
    pub undescribable: Vec<String>,
}

/// A finished export.
#[derive(Debug)]
pub struct Export {
    /// The set itself, ready to be rendered into the reply.
    pub set: Value,
    /// How many entries it holds.
    pub entries: usize,
    /// How many of those were disabled or spent when exported.
    pub disabled: usize,
    /// How many arm a **suspending** stop point.
    pub suspending: usize,
    pub dropped: Dropped,
}

/// Sort key that orders `bp_10` after `bp_2`.
///
/// A plain lexicographic sort on the id is deterministic but reads as shuffled, and this output is something a
/// person compares against a `list_stop_points` listing. The numeric suffix is the whole difference.
fn id_order(id: &str) -> (String, u64) {
    let cut = id.rfind('_').map_or(id.len(), |i| i + 1);
    let (prefix, digits) = id.split_at(cut);
    (prefix.to_string(), digits.parse().unwrap_or(u64::MAX))
}

/// Sort a map's entries into a stable, human-comparable order.
///
/// Deliberately not left to `HashMap` iteration order: an export that reorders itself between two runs of the
/// same session cannot be diffed, cannot be snapshot-tested, and reads as though something changed.
fn sorted_by_id<T>(map: &HashMap<String, T>) -> Vec<(&String, &T)> {
    let mut rows: Vec<_> = map.iter().collect();
    rows.sort_by_key(|(id, _)| id_order(id));
    rows
}

/// This session's stop points of one kind, in the same order (see [`sorted_by_id`]).
fn sorted_of_kind(
    session: &crate::session::DebugSession,
    kind: StopPointKind,
) -> Vec<&crate::stop_point::StopPoint> {
    let mut rows: Vec<_> = session.stop_points.values().filter(|sp| sp.kind() == kind).collect();
    rows.sort_by_key(|sp| id_order(&sp.id));
    rows
}

/// The arguments every kind shares, written into `args` in one place.
///
/// `trace_max_hits` carries the budget **that is left**, not the one originally asked for, because the
/// remaining budget is the only one the session keeps (TRACE-3 decrements in place). A set exported from a
/// stop point that has spent 190 of 200 hits re-arms with 10. That is stated in the tool description rather
/// than corrected here: inventing the original number would be a guess, and rounding it back up to the
/// default would silently widen what the caller armed.
#[allow(clippy::too_many_arguments)]
fn write_common(
    args: &mut Map<String, Value>,
    hit_count: Option<i32>,
    condition: Option<&str>,
    trace: bool,
    trace_expr: &[String],
    trace_budget: Option<u32>,
    trace_frames: usize,
    trace_max_length: Option<usize>,
) {
    args.insert("trace".to_string(), json!(trace));
    args.insert("trace_frames".to_string(), json!(trace_frames));
    if let Some(n) = hit_count {
        args.insert("hit_count".to_string(), json!(n));
    }
    if let Some(c) = condition {
        args.insert("condition".to_string(), json!(c));
    }
    if !trace_expr.is_empty() {
        args.insert("trace_expr".to_string(), json!(trace_expr));
    }
    if let Some(n) = trace_budget {
        args.insert("trace_max_hits".to_string(), json!(n));
    }
    if let Some(n) = trace_max_length {
        args.insert("trace_max_length".to_string(), json!(n));
    }
}

/// Build the set from a session's live stop points.
///
/// Reads the session rather than remembering the original calls, deliberately: what a caller wants back is the
/// investigation **as it now stands** — after the toggles, the added conditions and the spent budgets — not the
/// first version of it.
pub fn export(session: &crate::session::DebugSession) -> Export {
    let mut b = Builder::default();
    // Families first: each one owns member breakpoints that must not also be exported by exact name.
    let members = b.push_families(session);
    // One pass over the one collection, in the order [`StopPointKind::LISTING_ORDER`] declares — the same
    // order `list_stop_points` groups by, so a set and a listing cannot disagree about what came first.
    // It was seven per-kind passes over five maps (CLEAN-4).
    for kind in StopPointKind::LISTING_ORDER {
        if kind == StopPointKind::Monitor {
            // Folded rather than listed one by one — see [`Builder::push_monitors`].
            b.push_monitors(session);
        } else {
            for sp in sorted_of_kind(session, kind) {
                b.push_stop_point(sp, &members);
            }
        }
        // Deferred breakpoints export as ordinary line stops, and go where the line stops go.
        if kind == StopPointKind::Line {
            b.push_pending(session);
        }
    }
    b.finish()
}

/// The accumulator the export's passes write into.
///
/// A struct rather than a closure over local counters, and not for style: `export` was one function holding a
/// `FnMut` that borrowed three counters mutably, which meant every pass had to be inline (the borrow outlives
/// any attempt to split them out) and the whole thing came to 165 lines at cyclomatic complexity 25. Both of
/// this repo's limits, and both were telling the truth.
///
/// It was seven passes, one per kind plus the two things that are not stop points. CLEAN-4 left three:
/// [`Self::push_families`], [`Self::push_pending`] and [`Self::push_stop_point`], plus
/// [`Self::push_monitors`] for the one kind whose records are regrouped into the calls that armed them.
#[derive(Default)]
struct Builder {
    entries: Vec<Value>,
    dropped: Dropped,
    disabled: usize,
    suspending: usize,
}

impl Builder {
    /// Record one entry, counting the two things the reply warns about on the way past.
    fn push(&mut self, tool: &str, id: &str, enabled: bool, trace: bool, args: Map<String, Value>) {
        if !enabled {
            self.disabled += 1;
        }
        if !trace {
            self.suspending += 1;
        }
        self.entries.push(json!({"tool": tool, "from": id, "enabled": enabled, "args": Value::Object(args)}));
    }

    /// Note a filter being dropped, against the stop point it belonged to.
    fn drop_filters(&mut self, id: &str, instance: Option<u64>, thread: Option<u64>) {
        if instance.is_some() {
            self.dropped.instance.push(id.to_string());
        }
        if thread.is_some() {
            self.dropped.thread.push(id.to_string());
        }
    }

    fn finish(self) -> Export {
        let count = self.entries.len();
        Export {
            set: json!({FORMAT_KEY: FORMAT_VERSION, "entries": self.entries}),
            entries: count,
            disabled: self.disabled,
            suspending: self.suspending,
            dropped: self.dropped,
        }
    }

    /// Wildcard families, and the `bp_` ids they own so [`Self::push_stop_point`] can skip them.
    ///
    /// A family is ONE wildcard call that armed N breakpoints, so exporting both would re-arm the same
    /// locations twice — once through the pattern and once by exact name, which is BP-5's duplicate-stop-point
    /// shape created on purpose.
    fn push_families<'a>(&mut self, session: &'a crate::session::DebugSession) -> HashSet<&'a String> {
        let mut members: HashSet<&String> = HashSet::new();
        for (id, fam) in sorted_by_id(&session.pattern_sets) {
            members.extend(fam.members.iter());
            // The family keeps the pattern and the behaviour but not the locator — its members carry that, and
            // they are all re-pointed from one spec, so they all requested the same one.
            let line = fam
                .members
                .iter()
                .find_map(|m| session.stop_points.get(m))
                .and_then(StopPoint::line)
                .and_then(|b| b.arm_line);
            if line.is_none() && fam.method.is_none() {
                self.dropped.undescribable.push(id.clone());
                continue;
            }
            let mut args = Map::new();
            args.insert("class_pattern".to_string(), json!(fam.class_pattern));
            if let Some(l) = line {
                args.insert("line".to_string(), json!(l));
            }
            if let Some(m) = &fam.method {
                args.insert("method".to_string(), json!(m));
            }
            args.insert("max_classes".to_string(), json!(fam.max_classes));
            write_common(
                &mut args,
                fam.hit_count,
                fam.condition.as_deref(),
                fam.trace,
                &fam.trace_expr,
                fam.trace_budget,
                fam.trace_frames,
                fam.trace_max_length,
            );
            self.drop_filters(id, fam.instance_filter, fam.thread_filter);
            self.push("debug.set_line_stop", id, fam.enabled, fam.trace, args);
        }
        members
    }

    /// Deferred breakpoints, as ordinary line stops.
    ///
    /// Nothing marks them as pending, because "pending" is not a property of the request — it is what happens
    /// when the class is not loaded yet, and whether that is still true is for the next JVM to say (BP-7,
    /// ADR-0028). This is also the one kind whose locator was already stored unresolved.
    fn push_pending(&mut self, session: &crate::session::DebugSession) {
        for pb in &session.pending_breakpoints {
            let mut args = Map::new();
            args.insert("class_pattern".to_string(), json!(pb.class_pattern));
            if let Some(l) = pb.line {
                args.insert("line".to_string(), json!(l));
            }
            if let Some(m) = &pb.method {
                args.insert("method".to_string(), json!(m));
            }
            write_common(
                &mut args,
                pb.hit_count,
                pb.condition.as_deref(),
                pb.trace,
                &pb.trace_expr,
                pb.trace_budget,
                pb.trace_frames,
                pb.trace_max_length,
            );
            self.drop_filters(&pb.bp_id, pb.instance_filter, pb.thread_filter);
            self.push("debug.set_line_stop", &pb.bp_id, true, pb.trace, args);
        }
    }

    /// One stop point, as the `debug.set_*` call that would recreate it.
    ///
    /// **One function, not four.** The tool name and the locator arguments are all that differ between the
    /// kinds; the shared arguments were already written in one place by [`write_common`], and since CLEAN-4
    /// the values it reads come off one type rather than four that happened to agree on field names.
    fn push_stop_point(&mut self, sp: &StopPoint, family_members: &HashSet<&String>) {
        let mut args = Map::new();
        let tool = match &sp.armed_on {
            ArmedOn::Line(bp) => {
                // A family is ONE wildcard call that armed N breakpoints, so exporting a member by exact
                // name as well would re-arm the same location twice.
                if family_members.contains(&sp.id) {
                    return;
                }
                // `arm_line`/`arm_method` and NOT `line`/`method`: the latter pair is what the resolver
                // concluded, and writing a resolved method into a requested field narrows the entry against
                // any build where that line has moved. See `LineBreakpoint::arm_line` for the reasoning.
                if bp.arm_line.is_none() && bp.arm_method.is_none() {
                    self.dropped.undescribable.push(sp.id.clone());
                    return;
                }
                args.insert("class_pattern".to_string(), json!(bp.class_pattern));
                if let Some(l) = bp.arm_line {
                    args.insert("line".to_string(), json!(l));
                }
                if let Some(m) = &bp.arm_method {
                    args.insert("method".to_string(), json!(m));
                }
                "debug.set_line_stop"
            }
            ArmedOn::Exception(er) => {
                // An empty pattern is the every-exception form, which the argument spells as an absent
                // `class_pattern` rather than an empty string.
                if !er.class_pattern.is_empty() {
                    args.insert("class_pattern".to_string(), json!(er.class_pattern));
                }
                args.insert("caught".to_string(), json!(er.caught));
                args.insert("uncaught".to_string(), json!(er.uncaught));
                "debug.set_exception_stop"
            }
            ArmedOn::Watchpoint(wp) => {
                args.insert("class_name".to_string(), json!(wp.class_name));
                args.insert("field_name".to_string(), json!(wp.field_name));
                // One watch is one kind, so exactly one of the two is true. A caller who armed both got two
                // watchpoints and gets two entries, which re-arm to the same pair.
                args.insert("modify".to_string(), json!(wp.kind == jdwp_client::WatchKind::Modify));
                args.insert("access".to_string(), json!(wp.kind == jdwp_client::WatchKind::Access));
                "debug.set_field_stop"
            }
            ArmedOn::MethodExit(me) => {
                args.insert("class_pattern".to_string(), json!(me.class_pattern));
                if let Some(m) = &me.method {
                    args.insert("method".to_string(), json!(m));
                }
                if !me.exclude_classes.is_empty() {
                    args.insert("exclude_classes".to_string(), json!(me.exclude_classes));
                }
                "debug.set_method_exit_stop"
            }
            // Regrouped into the calls that armed them — see [`Self::push_monitors`], which is why this
            // one kind is not reached from here.
            ArmedOn::Monitor(_) => return,
        };
        write_common(
            &mut args,
            sp.hit_count,
            sp.condition.as_deref(),
            sp.trace,
            &sp.trace_expr,
            sp.trace_budget,
            sp.trace_frames,
            sp.trace_max_length,
        );
        self.drop_filters(&sp.id, sp.instance_filter, sp.thread_filter);
        self.push(tool, &sp.id, sp.enabled && !sp.spent, sp.trace, args);
    }

    /// Monitor requests, **regrouped into the calls that armed them**.
    ///
    /// One monitor stop point is one event kind, so a caller who armed the contended pair has two records.
    /// Exporting them as two calls would be wrong rather than merely verbose: `min_duration_ms` is **refused
    /// on a lone half** of a pair, because a duration is measured across two events and a single-kind request
    /// can never record one. Two entries would therefore be refused on the way back in, on a set this server
    /// itself produced.
    ///
    /// So records that agree on every setting are folded into one call carrying every kind they cover.
    fn push_monitors(&mut self, session: &crate::session::DebugSession) {
        let mut groups: Vec<(String, Vec<&StopPoint>)> = Vec::new();
        for sp in sorted_of_kind(session, StopPointKind::Monitor) {
            let Some(mon) = sp.monitor() else { continue };
            let key = format!(
                "{:?}|{:?}|{:?}|{}|{:?}|{}|{:?}|{:?}",
                sp.thread_filter,
                mon.monitor_class,
                mon.min_duration_ms,
                sp.trace,
                sp.hit_count,
                sp.trace_frames,
                sp.trace_max_length,
                sp.trace_expr
            );
            if let Some((_, found)) = groups.iter_mut().find(|(k, _)| *k == key) {
                found.push(sp);
            } else {
                groups.push((key, vec![sp]));
            }
            // Named against its own id even though the entry is shared, because that is the id the caller saw.
            self.drop_filters(&sp.id, None, sp.thread_filter);
        }

        for (_, members) in groups {
            let Some(first) = members.first() else { continue };
            let mut args = Map::new();
            let kinds: Vec<&str> =
                members.iter().filter_map(|m| m.monitor()).map(|m| m.kind.label()).collect();
            args.insert("kinds".to_string(), json!(kinds));
            if let Some(mon) = first.monitor() {
                if let Some(c) = &mon.monitor_class {
                    args.insert("monitor_class".to_string(), json!(c));
                }
                if let Some(n) = mon.min_duration_ms {
                    args.insert("min_duration_ms".to_string(), json!(n));
                }
            }
            write_common(
                &mut args,
                first.hit_count,
                None,
                first.trace,
                &first.trace_expr,
                first.trace_budget,
                first.trace_frames,
                first.trace_max_length,
            );
            let enabled = members.iter().all(|m| m.enabled && !m.spent);
            let ids = members.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join("+");
            self.push("debug.set_monitor_stop", &ids, enabled, first.trace, args);
        }
    }
}

/// Read a set back, refusing anything that is not one.
///
/// Every refusal names what was passed and what was expected, because the realistic mistake is not a corrupt
/// file — it is a caller passing the wrong thing entirely: the rendered `list_stop_points` listing, one entry
/// instead of the whole set, or a set from a future version of this server.
pub fn parse(raw: &str) -> Result<Vec<SetEntry>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            "The `set` argument is empty. Pass the block that debug.list_stop_points {export: true} \
                    returned."
                .to_string(),
        );
    }
    // Fenced by a client that helpfully wrapped it for display. Accepted rather than refused: the block IS
    // rendered inside a fence in the export reply, so a caller copying what they saw copies the fence.
    let unfenced = strip_code_fence(trimmed);
    let root: Value = serde_json::from_str(unfenced).map_err(|e| {
        format!(
            "The `set` argument is not JSON ({e}). Pass the block that debug.list_stop_points \
             {{export: true}} returned, not the rendered listing."
        )
    })?;
    let Some(obj) = root.as_object() else {
        return Err(format!(
            "A stop-point set is a JSON object with a `{FORMAT_KEY}` key. This is a {}.",
            type_name_of(&root)
        ));
    };
    let Some(version) = obj.get(FORMAT_KEY) else {
        return Err(format!(
            "This JSON has no `{FORMAT_KEY}` key, so it is not a stop-point set. Keys present: {}. Export \
             one with debug.list_stop_points {{export: true}}.",
            obj.keys().take(8).cloned().collect::<Vec<_>>().join(", ")
        ));
    };
    match version.as_u64() {
        Some(v) if v == FORMAT_VERSION => {}
        Some(v) if v > FORMAT_VERSION => {
            return Err(format!(
                "This set is format version {v} and this server reads version {FORMAT_VERSION}. It was \
                 exported by a NEWER build, so re-arming it here could arm something other than what it \
                 describes. Export a fresh set from this build."
            ));
        }
        _ => {
            return Err(format!(
                "`{FORMAT_KEY}` should be the format version as a number; it is {}.",
                type_name_of(version)
            ));
        }
    }
    let entries = obj
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "A stop-point set needs an `entries` array.".to_string())?;

    let mut out = Vec::with_capacity(entries.len());
    for (i, raw) in entries.iter().enumerate() {
        out.push(parse_entry(i, raw)?);
    }
    if out.is_empty() {
        return Err("This set has no entries, so there is nothing to arm. That is what an export of a \
                    session with no stop points looks like."
            .to_string());
    }
    Ok(out)
}

/// One entry, refused by index so a caller can find it in a set of thirty.
fn parse_entry(i: usize, raw: &Value) -> Result<SetEntry, String> {
    let obj = raw.as_object().ok_or_else(|| format!("entries[{i}] is not an object."))?;
    let tool = obj
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("entries[{i}] has no `tool` name."))?
        .to_string();
    if !ARMABLE_TOOLS.contains(&tool.as_str()) {
        return Err(format!(
            "entries[{i}] names `{tool}`, which debug.arm_stop_points will not call. A set may only arm stop \
             points, and the tools it may name are: {}. This is a whitelist rather than a check on this one \
             name — without it, a set would be a way to invoke anything in this server from a blob of JSON.",
            ARMABLE_TOOLS.join(", ")
        ));
    }
    let args = obj.get("args").cloned().unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return Err(format!("entries[{i}]'s `args` is not an object."));
    }
    Ok(SetEntry {
        tool,
        // Absent means enabled. A hand-written set should not have to say so, and the field exists to record
        // the one state that is NOT the default.
        enabled: obj.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        args,
    })
}

/// Strip one triple-backtick fence, with or without a `json` info string.
fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else { return s };
    let body = rest.split_once('\n').map_or("", |(_, b)| b);
    body.trim().strip_suffix("```").unwrap_or(body).trim()
}

/// What a JSON value is, for a refusal that tells the caller what they actually passed.
const fn type_name_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// What happened to one entry of a set being armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArmOutcome {
    /// Armed in the JVM.
    Armed,
    /// Accepted, but the class is not loaded yet, so it is waiting on a class-load watch (BP-7).
    Deferred,
    /// The tool refused it, with its own reason. Never fatal to the rest of the set.
    Refused(String),
    /// Exported as disabled or spent, and deliberately not armed.
    SkippedDisabled,
}

/// Render the per-entry outcomes of arming a set (DISC-14, #130).
///
/// **An aggregate alone will not do, and neither will silence.** #130 established that on this surface silence
/// has to mean *checked*, never *nobody looked*; a batch that reports `4 armed` when one of the four was
/// refused is the same defect wearing a total. So the counts lead and every entry that is not plainly armed is
/// named underneath with its reason.
///
/// Nothing aborts on a bad entry, following the wildcard/list precedent: a set is a batch, and one refused
/// location is a normal batch result rather than a reason to leave the other twenty-nine unarmed.
pub fn describe_arm_outcomes(outcomes: &[(String, ArmOutcome)]) -> String {
    let count = |want: &ArmOutcome| outcomes.iter().filter(|(_, o)| o == want).count();
    let armed = count(&ArmOutcome::Armed);
    let deferred = count(&ArmOutcome::Deferred);
    let skipped = count(&ArmOutcome::SkippedDisabled);
    let refused = outcomes.iter().filter(|(_, o)| matches!(o, ArmOutcome::Refused(_))).count();

    let mut out = format!("{armed} armed, {deferred} deferred, {refused} refused");
    if skipped > 0 {
        let _ = write!(out, ", {skipped} skipped (disabled when exported)");
    }
    out.push('\n');

    for (label, outcome) in outcomes {
        match outcome {
            // Named rather than listed: thirty armed lines would bury the four that need reading.
            ArmOutcome::Armed => {}
            ArmOutcome::Deferred => {
                let _ = writeln!(
                    out,
                    "   ⏳ {label} — deferred: the class is not loaded yet, so it is armed on a class-load \
                     watch and fires when it is."
                );
            }
            ArmOutcome::Refused(why) => {
                let _ = writeln!(out, "   🛑 {label} — refused: {why}");
            }
            ArmOutcome::SkippedDisabled => {
                let _ = writeln!(
                    out,
                    "   ⏸  {label} — NOT armed: it was disabled or spent when the set was exported. Its \
                     arguments are still in the set, so arm it with the tool named there if you want it back."
                );
            }
        }
    }
    out
}

/// The sentence that has to accompany every armed set: nothing here verified a line against bytecode.
///
/// DISC-14 (#130) again, and this is the case it was written for. A set carries a **line number**, which is a
/// claim about a build; re-arming it against a JVM running different bytecode resolves to whatever is now at
/// that line, and reports success. `debug.check_stale` is the thing that can answer it, and it is a separate
/// call rather than something this does per entry — thirty entries would be thirty round trips, and a caller
/// re-arming a set they exported ten seconds ago against the same JVM needs none of them.
///
/// Stated always, including when nothing armed, because the reading to prevent is "it armed, so the lines must
/// still be right".
pub fn describe_unverified_lines(armed: usize) -> String {
    if armed == 0 {
        return String::new();
    }
    "\n📐 Lines were NOT checked against the loaded bytecode. A set carries line numbers, which are a claim \
     about the build it was exported from — against a redeployed one they resolve to whatever is now on that \
     line, and arming reports success either way. Run debug.check_stale to settle it.\n"
        .to_string()
}

/// The safety sentence for a set that arms suspending stop points.
///
/// Not a refusal, following this project's posture of reporting a cost rather than forbidding it (#135's own
/// open question asked, and arming is not a write to the debuggee, so `read_only` has nothing to say here —
/// an invoking `condition` or `trace_expr` is still refused by the handler that receives it).
///
/// It is worth a sentence of its own because a set is the one call that can arm **many** suspending stop
/// points at once. Every other path arms one, where the caller is looking at the reply for that one.
pub fn describe_suspending(count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    format!(
        "\n⚠️  {count} of these are SUSPENDING stop points. On a shared JVM this one call is enough to freeze \
         it on the next hit of any of them — which is more exposure than arming them one at a time, where you \
         read a reply between each. The watchdog still applies. Consider trace:true in the set instead.\n"
    )
}

/// The sentence naming the filters an export could not carry.
///
/// Two sentences, not one, and the split is copied from `list_stop_points`' own two warnings: the cause differs
/// (a collected object against a retired thread), the fix differs (a fresh handle from `list_instances` against
/// a live id from `list_threads`), and a caller who has only ever used one of the two filters should not have
/// to work out which half is theirs.
pub fn describe_dropped(dropped: &Dropped) -> String {
    let mut out = String::new();
    if !dropped.instance.is_empty() {
        let _ = write!(
            out,
            "\n⚠️  Instance filter DROPPED from {}: {}. A JDWP object id is a weak reference to one object in \
             one JVM (ADR-0022), so it cannot mean anything in the next one — carrying it over would scope the \
             stop point to whatever now lives at that address. These entries are therefore BROADER than what \
             you exported. Take a fresh handle from debug.list_instances and re-apply it.\n",
            plural(dropped.instance.len(), "stop point"),
            dropped.instance.join(", ")
        );
    }
    if !dropped.thread.is_empty() {
        let _ = write!(
            out,
            "\n⚠️  Thread filter DROPPED from {}: {}. A JDWP thread id belongs to one JVM for the same reason, \
             and a pool that retires idle workers invalidates it even within one. These entries are BROADER \
             than what you exported. Read debug.list_threads for a live id and re-apply it.\n",
            plural(dropped.thread.len(), "stop point"),
            dropped.thread.join(", ")
        );
    }
    if !dropped.undescribable.is_empty() {
        let _ = write!(
            out,
            "\n⚠️  NOT exported: {}. A wildcard family whose members are all gone and which names no method \
             has no line and no method left to re-arm from, so there is nothing to write down. Re-arm it with \
             debug.set_line_stop.\n",
            dropped.undescribable.join(", ")
        );
    }
    out
}

/// `1 stop point` / `2 stop points`, so a reply does not read `1 stop point(s)`.
///
/// A caller-facing reply here is read by a person deciding whether their filter survived, and `1 stop point(s)`
/// is the tell that nobody read the sentence back.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// `1 entry` / `2 entries`, which the `-s` rule above cannot produce.
fn entries_phrase(n: usize) -> String {
    if n == 1 {
        "1 entry".to_string()
    } else {
        format!("{n} entries")
    }
}

/// The whole export reply: the warnings first, then the block to keep.
///
/// Warnings **before** the block on purpose. The block is long, and anything after it is past the point where a
/// reader has started copying — which is exactly where "these entries are broader than what you exported"
/// must not be.
pub fn render_export(export: &Export) -> String {
    let mut out = format!("📦 Stop-point set exported — {}.\n", entries_phrase(export.entries));
    if export.disabled > 0 {
        let _ = writeln!(
            out,
            "   {} of them were disabled or spent, and are recorded as such: debug.arm_stop_points will NOT \
             arm those.",
            export.disabled
        );
    }
    if export.suspending > 0 {
        let _ = writeln!(
            out,
            "   {} of them are SUSPENDING. Arming this set later is one call that can freeze a shared JVM on \
             the next hit of any of them — worth knowing now, while you still have the chance to re-arm them \
             with trace:true and export again.",
            export.suspending
        );
    }
    out.push_str(&describe_dropped(&export.dropped));
    let _ = write!(
        out,
        "\nNothing was written to disk — this server has no filesystem write path (BP-8). Store the block \
         below and pass it back verbatim as debug.arm_stop_points {{\"set\": \"…\"}}.\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(&export.set).unwrap_or_else(|_| "{}".to_string())
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The format is recognised by its own key, and everything that is not a set is refused by naming what it
    /// actually was. These are the realistic mistakes, not corruption.
    #[test]
    fn anything_that_is_not_a_set_is_refused_with_what_it_was_instead() {
        assert!(parse("").unwrap_err().contains("empty"));
        assert!(parse("not json at all").unwrap_err().contains("not JSON"));
        // The single most likely mistake: passing the rendered listing back.
        let listing = parse("📍 2 breakpoint(s), 0 deferred").unwrap_err();
        assert!(
            listing.contains("rendered listing"),
            "the refusal should name the likely mistake: {listing}"
        );

        let wrong_shape = parse("[1,2,3]").unwrap_err();
        assert!(wrong_shape.contains("array"), "it must say what was passed: {wrong_shape}");

        let no_key = parse(r#"{"entries":[]}"#).unwrap_err();
        assert!(no_key.contains(FORMAT_KEY) && no_key.contains("entries"), "{no_key}");
    }

    /// A set from a newer build is refused rather than read optimistically — it could describe a stop point
    /// this build would arm as something else.
    #[test]
    fn a_newer_format_version_is_refused_and_an_older_one_is_not_invented() {
        let newer = format!(r#"{{"{FORMAT_KEY}":{},"entries":[]}}"#, FORMAT_VERSION + 1);
        let err = parse(&newer).unwrap_err();
        assert!(err.contains("NEWER build"), "{err}");
        assert!(err.contains(&(FORMAT_VERSION + 1).to_string()), "it must quote the version it saw: {err}");

        let not_a_number = format!(r#"{{"{FORMAT_KEY}":"1","entries":[]}}"#);
        assert!(parse(&not_a_number).unwrap_err().contains("string"));
    }

    /// The whitelist is the whole reason this tool is not a generic RPC batch, so it is asserted as such.
    #[test]
    fn a_set_may_only_name_the_five_arming_tools() {
        let sneaky = format!(
            r#"{{"{FORMAT_KEY}":{FORMAT_VERSION},"entries":[{{"tool":"debug.disconnect","args":{{}}}}]}}"#
        );
        let err = parse(&sneaky).unwrap_err();
        assert!(err.contains("debug.disconnect"), "it must quote the tool it refused: {err}");
        assert!(err.contains("whitelist"), "and say it is a whitelist, not a check on this name: {err}");
        for tool in ARMABLE_TOOLS {
            assert!(err.contains(tool), "the refusal lists what IS allowed, so {tool} must appear: {err}");
        }
        // Every allowed tool really parses, or the whitelist is decorative.
        for tool in ARMABLE_TOOLS {
            let ok =
                format!(r#"{{"{FORMAT_KEY}":{FORMAT_VERSION},"entries":[{{"tool":"{tool}","args":{{}}}}]}}"#);
            assert_eq!(parse(&ok).expect("must parse").len(), 1, "{tool}");
        }
    }

    /// A fence is accepted because the export reply renders the block inside one, so a caller copying what
    /// they saw copies the fence. Refusing it would be refusing our own output.
    #[test]
    fn the_block_is_accepted_with_or_without_the_fence_it_was_shown_in() {
        let bare = format!(
            r#"{{"{FORMAT_KEY}":{FORMAT_VERSION},"entries":[{{"tool":"debug.set_line_stop","args":{{}}}}]}}"#
        );
        let fenced = format!("```json\n{bare}\n```");
        assert_eq!(parse(&bare).expect("bare").len(), 1);
        assert_eq!(parse(&fenced).expect("fenced").len(), 1, "our own rendering must round-trip");
    }

    /// `enabled` absent means enabled: a hand-written set should not have to state the default, and the field
    /// exists to record the state that is not it.
    #[test]
    fn a_missing_enabled_flag_means_enabled() {
        let set = format!(
            r#"{{"{FORMAT_KEY}":{FORMAT_VERSION},"entries":[
                 {{"tool":"debug.set_line_stop","args":{{}}}},
                 {{"tool":"debug.set_line_stop","enabled":false,"args":{{}}}}]}}"#
        );
        let entries = parse(&set).expect("parse");
        assert!(entries[0].enabled, "absent means enabled");
        assert!(!entries[1].enabled, "and an explicit false is honoured");
    }

    /// An empty set is refused rather than reported as "0 armed", because the two readings differ: one is a
    /// caller who exported nothing, the other is a caller who passed the wrong thing.
    #[test]
    fn an_empty_entry_list_is_refused_rather_than_armed_as_nothing() {
        let empty = format!(r#"{{"{FORMAT_KEY}":{FORMAT_VERSION},"entries":[]}}"#);
        assert!(parse(&empty).unwrap_err().contains("nothing to arm"));
    }

    /// DISC-14: the counts lead, and every entry that is not plainly armed is named with its reason. An
    /// aggregate that hid a refusal would be the defect #130 was filed about.
    #[test]
    fn arm_outcomes_lead_with_counts_and_then_name_everything_that_is_not_armed() {
        let outcomes = vec![
            ("bp_1 com.x.Foo:42".to_string(), ArmOutcome::Armed),
            ("bp_2 com.x.Bar:7".to_string(), ArmOutcome::Deferred),
            ("bp_3 com.x.Gone:1".to_string(), ArmOutcome::Refused("no such line".to_string())),
            ("bp_4 com.x.Off:3".to_string(), ArmOutcome::SkippedDisabled),
        ];
        let out = describe_arm_outcomes(&outcomes);
        assert!(out.starts_with("1 armed, 1 deferred, 1 refused"), "counts first: {out}");
        assert!(out.contains("1 skipped"), "and the skipped count when there is one: {out}");
        // The armed one is NOT listed; the other three are. Thirty armed lines would bury the ones to read.
        assert!(!out.contains("bp_1"), "an armed entry needs no line of its own: {out}");
        for id in ["bp_2", "bp_3", "bp_4"] {
            assert!(out.contains(id), "{id} must be named: {out}");
        }
        assert!(out.contains("no such line"), "a refusal carries the tool's own reason: {out}");
    }

    /// The staleness sentence is stated whenever anything armed, because the reading to prevent is "it armed,
    /// so the lines must still be right".
    #[test]
    fn nothing_claims_the_lines_were_checked() {
        assert_eq!(describe_unverified_lines(0), "", "nothing armed, so there is nothing to disclaim");
        let note = describe_unverified_lines(3);
        assert!(note.contains("check_stale"), "it must name the tool that CAN answer it: {note}");
        assert!(note.contains("NOT checked"), "and be unambiguous that it did not: {note}");
    }

    /// Dropped filters get two sentences rather than one, following `list_stop_points`' own split.
    #[test]
    fn a_dropped_filter_says_the_stop_point_is_now_broader() {
        assert_eq!(describe_dropped(&Dropped::default()), "", "nothing dropped, nothing said");

        let both = Dropped {
            instance: vec!["bp_1".to_string()],
            thread: vec!["bp_2".to_string(), "mexit_1".to_string()],
            undescribable: vec![],
        };
        let out = describe_dropped(&both);
        assert!(out.contains("bp_1") && out.contains("mexit_1"), "every id is named: {out}");
        // The consequence, not just the fact. A filter silently omitted turns a narrow stop point broad.
        assert_eq!(out.matches("BROADER").count(), 2, "each half states the consequence: {out}");
        assert!(
            out.contains("list_instances") && out.contains("list_threads"),
            "each names its own fix: {out}"
        );
        assert!(out.contains("1 stop point:") || out.contains("1 stop point"), "no `1 stop point(s)`: {out}");
        assert!(!out.contains("point(s)"), "the plural is resolved, not deferred to the reader: {out}");
    }

    /// Suspending stop points get a warning and not a refusal — reporting the cost is this project's posture —
    /// but a set is the one call that can arm many at once, which is why it says so at all.
    #[test]
    fn a_set_of_suspending_stop_points_is_warned_about_and_not_refused() {
        assert_eq!(describe_suspending(0), "");
        let warn = describe_suspending(4);
        assert!(warn.contains('4') && warn.contains("SUSPENDING"), "{warn}");
        assert!(warn.contains("one call"), "the point is the multiplicity, not the kind: {warn}");
        assert!(warn.contains("trace:true"), "and it names the cheaper alternative: {warn}");
    }

    /// `bp_10` sorts after `bp_2`. A lexicographic sort is deterministic and reads as shuffled, and this output
    /// is compared by eye against a listing.
    #[test]
    fn entries_are_ordered_by_number_and_not_by_string() {
        let mut ids = vec!["bp_10", "bp_2", "mexit_1", "bp_1"];
        ids.sort_by_key(|i| id_order(i));
        assert_eq!(ids, vec!["bp_1", "bp_2", "bp_10", "mexit_1"]);
    }
}
