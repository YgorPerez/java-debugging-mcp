// JDWP event handling
//
// Events are sent from the JVM to notify about breakpoints, steps, etc.

use crate::commands::event_kinds;
use crate::protocol::JdwpResult;
use crate::reader::{read_i32, read_string, read_u64, read_u8};
use crate::types::{FieldId, Location, ObjectId, ReferenceTypeId, ThreadId, Value};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Composite event packet (can contain multiple events)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSet {
    pub suspend_policy: u8,
    pub events: Vec<Event>,
}

/// Single event within an event set
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub kind: u8,
    pub request_id: i32,
    pub details: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventKind {
    VMStart {
        thread: ThreadId,
    },
    VMDeath,
    ThreadStart {
        thread: ThreadId,
    },
    ThreadDeath {
        thread: ThreadId,
    },
    ClassPrepare {
        thread: ThreadId,
        ref_type: ReferenceTypeId,
        signature: String,
        status: i32,
    },
    Breakpoint {
        thread: ThreadId,
        location: Location,
    },
    Step {
        thread: ThreadId,
        location: Location,
    },
    Exception {
        thread: ThreadId,
        location: Location,
        exception: ObjectId,
        catch_location: Option<Location>,
    },
    /// A method is returning. `location` is the return site, so a method with several `return`
    /// statements says which one was taken.
    ///
    /// There is deliberately no `MethodEntry`: a `METHOD_ENTRY` request with a `ClassMatch` fires on
    /// every method of every matching class — the noisiest event in JDWP — and "what calls this?" is
    /// now answered far more cheaply by a traced breakpoint's caller chain (TRACE-5). A decoded variant
    /// nothing can arm only implies a capability that isn't there.
    MethodExit {
        thread: ThreadId,
        location: Location,
        /// What the method is returning, present only when the request was armed as
        /// `METHOD_EXIT_WITH_RETURN_VALUE` (kind 42). `None` for a plain `METHOD_EXIT` (kind 41),
        /// which a JVM below JDWP 1.6 is all you can get.
        return_value: Option<Value>,
    },
    /// A watched field was read.
    FieldAccess {
        field: FieldEvent,
    },
    /// A watched field is about to be written. The event fires *before* the store commits, so the
    /// field still holds its old value while the thread is suspended — that is how the old→new pair
    /// is reported.
    FieldModification {
        field: FieldEvent,
        /// JDWP's `valueToBe` — the value the write will store.
        new_value: Value,
    },
    /// A thread has begun **blocking** on a monitor another thread owns
    /// (`MONITOR_CONTENDED_ENTER`, 43). The thread is off the pool from here until the matching
    /// [`MonitorContendedEntered`](Self::MonitorContendedEntered) arrives.
    MonitorContendedEnter {
        monitor: MonitorEvent,
    },
    /// A thread that was blocking has **acquired** the monitor (`MONITOR_CONTENDED_ENTERED`, 44).
    ///
    /// **This event carries no timing of any kind.** How long the thread was blocked — the actual
    /// question a contention diagnosis asks — is on neither half of the pair, so it can only be had by
    /// timestamping the `ENTER` on this side and matching it here. See `mcp-server`'s monitor pairing and
    /// ADR-0035: the resulting figure is a *debugger* measurement and every reply that prints one says so.
    MonitorContendedEntered {
        monitor: MonitorEvent,
    },
    /// A thread is about to `Object.wait()` (`MONITOR_WAIT`, 45).
    MonitorWait {
        monitor: MonitorEvent,
        /// JDWP's `timeout` — the number of milliseconds the caller **asked** `wait(…)` for, `0` for an
        /// untimed wait. It is the argument, not a measurement: a `wait(5000)` that returns after 3 ms
        /// still reports 5000 here.
        timeout: i64,
    },
    /// A thread's `Object.wait()` has returned (`MONITOR_WAITED`, 46).
    MonitorWaited {
        monitor: MonitorEvent,
        /// Whether the wait ended because the timeout expired rather than because of a `notify`. The one
        /// piece of outcome the wire does carry, and the difference between "nobody signalled it" and
        /// "it was signalled" — which are opposite diagnoses.
        timed_out: bool,
    },
    Unknown {
        kind: u8,
    },
}

/// Which monitor was contended, by which thread, at what code.
///
/// The context all four monitor events carry. They differ only in what (if anything) follows it, so it
/// lives in one struct, exactly as [`FieldEvent`] does for the two field events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorEvent {
    /// The thread that blocked, acquired, waited or finished waiting.
    pub thread: ThreadId,
    /// The code that was executing, **not** where the monitor's type is declared — for a
    /// `synchronized` block, the block's own location.
    pub location: Location,
    /// The monitor object itself. Arrives as a tagged-objectID and is a **weak** reference like every
    /// other object id here (ADR-0022), so a pairing keyed on it must not assume it stays readable.
    pub monitor: ObjectId,
}

/// Which field was touched, by what code, on which object — the context both field events carry.
/// They differ only in whether a pending value comes with it, so it lives in one struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldEvent {
    pub thread: ThreadId,
    /// The code that touched the field, *not* where the field is declared.
    pub location: Location,
    /// The type declaring the field.
    pub ref_type: ReferenceTypeId,
    pub field_id: FieldId,
    /// The instance whose field was touched; 0 for a static field.
    pub object: ObjectId,
}

// Event request modifiers
#[derive(Debug, Clone)]
pub enum EventModifier {
    Count(i32),
    ThreadOnly(ThreadId),
    ClassOnly(ReferenceTypeId),
    ClassMatch(String),
    ClassExclude(String),
    LocationOnly(Location),
    ExceptionOnly { ref_type: ReferenceTypeId, caught: bool, uncaught: bool },
    FieldOnly { ref_type: ReferenceTypeId, field_id: FieldId },
    Step { thread: ThreadId, size: i32, depth: i32 },
    InstanceOnly(ObjectId),
}

/// Parse an event packet from JDWP
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn parse_event_packet(data: &[u8]) -> JdwpResult<EventSet> {
    let mut buf = data;

    // Read suspend policy
    let suspend_policy = read_u8(&mut buf)?;

    // Read number of events
    let event_count = read_i32(&mut buf)?;

    let mut events = Vec::with_capacity(usize::try_from(event_count).unwrap_or(0));

    for _ in 0..event_count {
        let kind = read_u8(&mut buf)?;
        let request_id = read_i32(&mut buf)?;

        let details = parse_event_details(kind, &mut buf)?;

        events.push(Event { kind, request_id, details });
    }

    Ok(EventSet { suspend_policy, events })
}

/// Dispatch a single event's kind-specific payload to the matching parser.
///
/// The **stop-point** kinds are here — the ones a debugger arms deliberately and that carry a thread and a
/// location — while the VM's own lifecycle notifications and the monitor family are delegated. Split that
/// way because the table had grown past the point where one `match` could be read at a glance, and because
/// those are the two groups whose members share a shape: [`parse_vm_lifecycle_event`]'s carry no location,
/// and [`parse_monitor_event`]'s all share one prefix.
fn parse_event_details(kind: u8, buf: &mut &[u8]) -> JdwpResult<EventKind> {
    if let Some(parsed) = parse_vm_lifecycle_event(kind, buf) {
        return parsed;
    }
    if let Some(parsed) = parse_monitor_event(kind, buf) {
        return parsed;
    }
    match kind {
        event_kinds::BREAKPOINT => parse_breakpoint_event(buf),
        event_kinds::SINGLE_STEP => parse_step_event(buf),
        event_kinds::EXCEPTION => parse_exception_event(buf),
        event_kinds::FIELD_ACCESS => parse_field_access_event(buf),
        event_kinds::FIELD_MODIFICATION => parse_field_modification_event(buf),
        event_kinds::METHOD_EXIT => parse_method_exit_event(buf, false),
        event_kinds::METHOD_EXIT_WITH_RETURN_VALUE => parse_method_exit_event(buf, true),
        _ => {
            warn!("Unsupported event kind: {}", kind);
            Ok(EventKind::Unknown { kind })
        }
    }
}

/// The VM's own lifecycle notifications, which arrive whether anything asked for them or not. `None` for
/// any other kind, so the caller keeps dispatching.
///
/// These share a shape: none of them carries a location, which is why `event_location` on the `mcp-server`
/// side answers `None` for every one of them.
fn parse_vm_lifecycle_event(kind: u8, buf: &mut &[u8]) -> Option<JdwpResult<EventKind>> {
    match kind {
        event_kinds::VM_START => Some(parse_vm_start_event(buf)),
        event_kinds::VM_DEATH => Some(Ok(EventKind::VMDeath)),
        event_kinds::THREAD_START => Some(parse_thread_start_event(buf)),
        event_kinds::THREAD_DEATH => Some(parse_thread_death_event(buf)),
        event_kinds::CLASS_PREPARE => Some(parse_class_prepare_event(buf)),
        _ => None,
    }
}

/// The four monitor kinds (DUMP-7, #96). `None` for any other kind.
///
/// Grouped because they share [`parse_monitor_event_head`] — the tagged-objectID prefix and its trap — and
/// differ only in a trailing field of a different Rust type each.
fn parse_monitor_event(kind: u8, buf: &mut &[u8]) -> Option<JdwpResult<EventKind>> {
    match kind {
        event_kinds::MONITOR_CONTENDED_ENTER => {
            Some(parse_monitor_event_head(buf).map(|monitor| EventKind::MonitorContendedEnter { monitor }))
        }
        event_kinds::MONITOR_CONTENDED_ENTERED => {
            Some(parse_monitor_event_head(buf).map(|monitor| EventKind::MonitorContendedEntered { monitor }))
        }
        event_kinds::MONITOR_WAIT => Some(parse_monitor_wait_event(buf)),
        event_kinds::MONITOR_WAITED => Some(parse_monitor_waited_event(buf)),
        _ => None,
    }
}

fn parse_breakpoint_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let thread = read_u64(buf)?;
    let location = read_location(buf)?;
    Ok(EventKind::Breakpoint { thread, location })
}

fn parse_step_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let thread = read_u64(buf)?;
    let location = read_location(buf)?;
    Ok(EventKind::Step { thread, location })
}

fn parse_vm_start_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let thread = read_u64(buf)?;
    Ok(EventKind::VMStart { thread })
}

fn parse_thread_start_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let thread = read_u64(buf)?;
    Ok(EventKind::ThreadStart { thread })
}

fn parse_thread_death_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let thread = read_u64(buf)?;
    Ok(EventKind::ThreadDeath { thread })
}

fn parse_class_prepare_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    // thread, refTypeTag (byte, discarded), typeID, signature, status
    let thread = read_u64(buf)?;
    let _ref_type_tag = read_u8(buf)?;
    let ref_type = read_u64(buf)?;
    let signature = read_string(buf)?;
    let status = read_i32(buf)?;
    Ok(EventKind::ClassPrepare { thread, ref_type, signature, status })
}

fn parse_exception_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    // thread, throw location, exception (tagged-objectID), catch location.
    // The catch location is all-zero when the exception is uncaught.
    let thread = read_u64(buf)?;
    let location = read_location(buf)?;
    let _exc_tag = read_u8(buf)?;
    let exception = read_u64(buf)?;
    let catch = read_location(buf)?;
    let catch_location =
        if catch.class_id == 0 && catch.method_id == 0 && catch.index == 0 { None } else { Some(catch) };
    Ok(EventKind::Exception { thread, location, exception, catch_location })
}

/// Read the prefix both field events share: thread, the touching location, the declaring type, the
/// field, and the instance involved (a tagged-objectID that is null for a static field).
fn parse_field_event_head(buf: &mut &[u8]) -> JdwpResult<FieldEvent> {
    let thread = read_u64(buf)?;
    let location = read_location(buf)?;
    let _ref_type_tag = read_u8(buf)?;
    let ref_type = read_u64(buf)?;
    let field_id = read_u64(buf)?;
    let _obj_tag = read_u8(buf)?;
    let object = read_u64(buf)?;
    Ok(FieldEvent { thread, location, ref_type, field_id, object })
}

fn parse_field_access_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    Ok(EventKind::FieldAccess { field: parse_field_event_head(buf)? })
}

fn parse_field_modification_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let field = parse_field_event_head(buf)?;
    // valueToBe: a tagged value carrying what the pending write will store.
    let tag = read_u8(buf)?;
    let new_value = Value { tag, data: crate::reader::read_value_by_tag(tag, buf)? };
    Ok(EventKind::FieldModification { field, new_value })
}

/// Read the prefix all four monitor events share: the thread, the monitor object (a tagged-objectID),
/// and the location of the code involved.
///
/// **Note the field ORDER, which is not the field events' order.** A monitor event puts its object
/// *before* the location; `parse_field_event_head` above puts the location first. Getting it the other
/// way round does not fail — a location's leading typeTag byte reads as the object's tag and the whole
/// remainder shifts, which for [`parse_monitor_wait_event`] means a garbage timeout and inside a
/// composite means the *next* event desynchronises.
///
/// One head parser rather than four copies, and rather than a shape enum: the trap this exists to
/// contain is entirely in the shared prefix — the tag byte — while the two tails differ in Rust *type*
/// (`i64` against `bool`), so an enum would only move the match one level out. This is the same split
/// `parse_field_event_head` uses for the same reason.
fn parse_monitor_event_head(buf: &mut &[u8]) -> JdwpResult<MonitorEvent> {
    let thread = read_u64(buf)?;
    // The monitor is a tagged-objectID: one tag byte (always `L`, an object) and then the id. Dropping
    // the tag read is the mistake that silently shifts every field after it.
    let _monitor_tag = read_u8(buf)?;
    let monitor = read_u64(buf)?;
    let location = read_location(buf)?;
    Ok(MonitorEvent { thread, location, monitor })
}

fn parse_monitor_wait_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let monitor = parse_monitor_event_head(buf)?;
    // The timeout the caller passed to `wait(…)`, not how long it waited. Signed, because that is how
    // JDWP declares it and how `Object.wait(long)` takes it.
    let timeout = crate::reader::read_i64(buf)?;
    Ok(EventKind::MonitorWait { monitor, timeout })
}

fn parse_monitor_waited_event(buf: &mut &[u8]) -> JdwpResult<EventKind> {
    let monitor = parse_monitor_event_head(buf)?;
    let timed_out = read_u8(buf)? != 0;
    Ok(EventKind::MonitorWaited { monitor, timed_out })
}

/// Parse a `METHOD_EXIT` (kind 41) or `METHOD_EXIT_WITH_RETURN_VALUE` (kind 42) event.
///
/// The two differ only by a trailing tagged value, so `with_return_value` decides whether to read it.
/// Reading it when the request did not ask for it would consume the next event's bytes.
fn parse_method_exit_event(buf: &mut &[u8], with_return_value: bool) -> JdwpResult<EventKind> {
    let thread = read_u64(buf)?;
    let location = read_location(buf)?;
    let return_value = if with_return_value {
        let tag = read_u8(buf)?;
        Some(Value { tag, data: crate::reader::read_value_by_tag(tag, buf)? })
    } else {
        None
    };
    Ok(EventKind::MethodExit { thread, location, return_value })
}

/// Read a location from the buffer
fn read_location(buf: &mut &[u8]) -> JdwpResult<Location> {
    let type_tag = read_u8(buf)?;
    let class_id = read_u64(buf)?;
    let method_id = read_u64(buf)?;
    let index = read_u64(buf)?;

    Ok(Location { type_tag, class_id, method_id, index })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::event_kinds;

    /// Build an event-packet body: suspend policy, event count, then each event's bytes.
    fn packet(suspend_policy: u8, events: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![suspend_policy];
        out.extend_from_slice(&i32::try_from(events.len()).unwrap_or(0).to_be_bytes());
        for e in events {
            out.extend_from_slice(e);
        }
        out
    }

    /// A JDWP location: typeTag, classID, methodID, code index.
    fn location(class: u64, method: u64, index: u64) -> Vec<u8> {
        let mut out = vec![1];
        out.extend_from_slice(&class.to_be_bytes());
        out.extend_from_slice(&method.to_be_bytes());
        out.extend_from_slice(&index.to_be_bytes());
        out
    }

    fn breakpoint_event(request_id: i32, thread: u64) -> Vec<u8> {
        let mut out = vec![event_kinds::BREAKPOINT];
        out.extend_from_slice(&request_id.to_be_bytes());
        out.extend_from_slice(&thread.to_be_bytes());
        out.extend_from_slice(&location(0x11, 0x22, 3));
        out
    }

    /// A `FIELD_MODIFICATION` event, whose trailing `valueToBe` is the one place an event parser reads a
    /// tagged value — and the path that used to panic on a short buffer instead of erroring.
    fn field_modification_event(new_value: i32) -> Vec<u8> {
        let mut out = vec![event_kinds::FIELD_MODIFICATION];
        out.extend_from_slice(&7i32.to_be_bytes()); // requestId
        out.extend_from_slice(&0x1u64.to_be_bytes()); // thread
        out.extend_from_slice(&location(0x11, 0x22, 3));
        out.push(1); // refTypeTag
        out.extend_from_slice(&0x33u64.to_be_bytes()); // refType
        out.extend_from_slice(&0x44u64.to_be_bytes()); // fieldId
        out.push(crate::reader::value_tags::OBJECT); // object tag
        out.extend_from_slice(&0u64.to_be_bytes()); // object (0 = static field)
        out.push(crate::reader::value_tags::INT);
        out.extend_from_slice(&new_value.to_be_bytes());
        out
    }

    /// A `METHOD_EXIT` (41) or `METHOD_EXIT_WITH_RETURN_VALUE` (42) event. Kind 42 carries a trailing
    /// tagged value; kind 41 does not, and reading one anyway would eat the next event's bytes.
    fn method_exit_event(with_return_value: bool, returned: i32) -> Vec<u8> {
        let mut out = vec![if with_return_value {
            event_kinds::METHOD_EXIT_WITH_RETURN_VALUE
        } else {
            event_kinds::METHOD_EXIT
        }];
        out.extend_from_slice(&9i32.to_be_bytes()); // requestId
        out.extend_from_slice(&0x1u64.to_be_bytes()); // thread
        out.extend_from_slice(&location(0x55, 0x66, 12));
        if with_return_value {
            out.push(crate::reader::value_tags::INT);
            out.extend_from_slice(&returned.to_be_bytes());
        }
        out
    }

    /// METH-1: kind 42 yields the returned value; kind 41 yields the return site with no value. Getting
    /// this wrong is not a missing field but a desynchronised buffer — the value's bytes would be read
    /// as the next event's header.
    #[test]
    fn method_exit_parses_with_and_without_a_return_value() {
        let with = parse_event_packet(&packet(1, &[method_exit_event(true, 42)])).expect("well-formed");
        match with.events.first().map(|e| &e.details) {
            Some(EventKind::MethodExit { location, return_value: Some(v), .. }) => {
                assert_eq!(location.method_id, 0x66, "the return site is the hit location");
                assert!(matches!(v.data, crate::types::ValueData::Int(42)), "got {:?}", v.data);
            }
            other => panic!("expected a method exit with a value, got {other:?}"),
        }

        let without = parse_event_packet(&packet(1, &[method_exit_event(false, 0)])).expect("well-formed");
        assert!(
            matches!(
                without.events.first().map(|e| &e.details),
                Some(EventKind::MethodExit { return_value: None, .. })
            ),
            "kind 41 carries no value, got {:?}",
            without.events.first().map(|e| &e.details)
        );

        // Two kind-42 events back to back: the second only parses if the first consumed its value and
        // nothing more. This is the assertion that catches a length mistake in the tagged-value read.
        let pair = parse_event_packet(&packet(1, &[method_exit_event(true, 7), method_exit_event(true, 8)]))
            .expect("well-formed");
        assert_eq!(pair.events.len(), 2, "the first event must consume exactly its own bytes");
    }

    #[test]
    fn an_empty_event_set_parses_as_zero_events() {
        let set = parse_event_packet(&packet(2, &[])).expect("an empty set is well-formed");
        assert_eq!(set.suspend_policy, 2);
        assert!(set.events.is_empty());
    }

    #[test]
    fn a_well_formed_set_parses_every_event() {
        let wire = packet(1, &[breakpoint_event(5, 0xabc), field_modification_event(42)]);
        let set = parse_event_packet(&wire).expect("well-formed");
        assert_eq!(set.events.len(), 2);
        match &set.events[0].details {
            EventKind::Breakpoint { thread, location } => {
                assert_eq!(*thread, 0xabc);
                assert_eq!(location.method_id, 0x22);
            }
            other => panic!("expected a breakpoint, got {other:?}"),
        }
        match &set.events[1].details {
            EventKind::FieldModification { field, new_value } => {
                assert_eq!(field.field_id, 0x44);
                assert!(matches!(new_value.data, crate::types::ValueData::Int(42)));
            }
            other => panic!("expected a field modification, got {other:?}"),
        }
    }

    /// An event kind we don't parse must degrade to `Unknown`, not fail the whole set: the JVM sends
    /// kinds we never requested (frame pops, class unloads), and dropping the set would lose
    /// the events beside it.
    #[test]
    fn an_unhandled_event_kind_becomes_unknown_rather_than_an_error() {
        // FRAME_POP is a real JDWP kind this client never requests and does not parse — the honest case,
        // rather than a number the protocol doesn't define. It used to be `MONITOR_WAIT`, which DUMP-7
        // (#96) decoded: an example chosen for being unhandled has to be re-chosen when it stops being.
        let mut ev = vec![event_kinds::FRAME_POP];
        ev.extend_from_slice(&1i32.to_be_bytes());
        let set = parse_event_packet(&packet(0, &[ev])).expect("an unhandled kind is not a parse failure");
        assert!(
            matches!(set.events.first().map(|e| &e.details), Some(EventKind::Unknown { kind })
                if *kind == event_kinds::FRAME_POP),
            "expected Unknown, got {:?}",
            set.events.first().map(|e| &e.details)
        );
    }

    /// A monitor event of any of the four kinds: thread, the monitor as a **tagged**-objectID, location,
    /// then whichever tail the kind carries.
    fn monitor_event(kind: u8, monitor: u64, tail: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&11i32.to_be_bytes()); // requestId
        out.extend_from_slice(&0x7fu64.to_be_bytes()); // thread
        out.push(crate::reader::value_tags::OBJECT); // the tag byte the head parser must consume
        out.extend_from_slice(&monitor.to_be_bytes());
        out.extend_from_slice(&location(0x99, 0xaa, 4));
        out.extend_from_slice(tail);
        out
    }

    /// DUMP-7 (#96): all four kinds decode, and each reports the monitor object rather than reading the
    /// location's typeTag as part of it.
    ///
    /// The two-in-one-packet assertions are the load-bearing ones. Dropping the tagged-objectID's tag
    /// byte, or reading `MONITOR_WAIT`'s trailing `long` for a kind that does not carry one, both leave a
    /// single event *looking* fine while shifting everything after it — so the mistake only shows up as a
    /// second event that fails to parse or arrives with garbage.
    #[test]
    fn every_monitor_event_kind_decodes_with_its_own_tail() {
        let enter = parse_event_packet(&packet(
            1,
            &[monitor_event(event_kinds::MONITOR_CONTENDED_ENTER, 0x1234, &[])],
        ))
        .expect("well-formed");
        match enter.events.first().map(|e| &e.details) {
            Some(EventKind::MonitorContendedEnter { monitor }) => {
                assert_eq!(monitor.monitor, 0x1234, "the monitor object, not the location's typeTag");
                assert_eq!(monitor.thread, 0x7f);
                assert_eq!(monitor.location.method_id, 0xaa);
            }
            other => panic!("expected a contended enter, got {other:?}"),
        }

        // ENTER and ENTERED back to back: the second only parses if the first consumed exactly its own
        // bytes — the pair a debugger-measured elapsed is computed from, so both halves must survive one
        // composite.
        let pair = parse_event_packet(&packet(
            1,
            &[
                monitor_event(event_kinds::MONITOR_CONTENDED_ENTER, 0x1234, &[]),
                monitor_event(event_kinds::MONITOR_CONTENDED_ENTERED, 0x1234, &[]),
            ],
        ))
        .expect("well-formed");
        assert_eq!(pair.events.len(), 2, "an enter must consume exactly its own bytes");
        assert!(
            matches!(&pair.events[1].details, EventKind::MonitorContendedEntered { monitor } if monitor.monitor == 0x1234),
            "got {:?}",
            pair.events[1].details
        );

        // WAIT's trailing `long` is the requested timeout, and WAITED's is a one-byte flag. A second
        // event after each is what catches reading the wrong width.
        let waits = parse_event_packet(&packet(
            1,
            &[
                monitor_event(event_kinds::MONITOR_WAIT, 0x55, &5000i64.to_be_bytes()),
                monitor_event(event_kinds::MONITOR_WAITED, 0x55, &[1]),
                monitor_event(event_kinds::MONITOR_WAITED, 0x55, &[0]),
            ],
        ))
        .expect("well-formed");
        assert_eq!(waits.events.len(), 3, "each tail must be consumed at its own width");
        assert!(
            matches!(&waits.events[0].details, EventKind::MonitorWait { timeout: 5000, .. }),
            "got {:?}",
            waits.events[0].details
        );
        assert!(
            matches!(&waits.events[1].details, EventKind::MonitorWaited { timed_out: true, .. }),
            "got {:?}",
            waits.events[1].details
        );
        assert!(
            matches!(&waits.events[2].details, EventKind::MonitorWaited { timed_out: false, .. }),
            "a notified wait did not time out, got {:?}",
            waits.events[2].details
        );
    }

    /// Every truncation of a valid packet must error rather than panic. This is the assertion that
    /// would have caught the pre-`reader` behaviour: a short `valueToBe` panicked the event-loop task,
    /// killing the whole debug session instead of reporting a malformed reply.
    #[test]
    fn every_truncation_of_a_packet_errors_instead_of_panicking() {
        for event in [
            breakpoint_event(5, 0xabc),
            field_modification_event(42),
            method_exit_event(true, 42),
            monitor_event(event_kinds::MONITOR_WAIT, 0x55, &5000i64.to_be_bytes()),
        ] {
            let wire = packet(1, &[event]);
            for keep in 0..wire.len() {
                let short = &wire[..keep];
                // The count field can read as 0 before it is complete, which is a legitimate empty set.
                let parsed = parse_event_packet(short);
                if let Ok(set) = parsed {
                    assert!(
                        set.events.is_empty(),
                        "{keep} of {} bytes parsed as {} complete event(s)",
                        wire.len(),
                        set.events.len()
                    );
                }
            }
        }
    }

    /// A claimed event count far larger than the data must not pre-allocate for it or panic — it must
    /// fail when the events run out.
    #[test]
    fn a_lying_event_count_errors_rather_than_over_reading() {
        let mut wire = vec![1u8];
        wire.extend_from_slice(&1000i32.to_be_bytes());
        wire.extend_from_slice(&breakpoint_event(5, 0xabc));
        assert!(parse_event_packet(&wire).is_err(), "1000 claimed, 1 supplied");
    }
}
