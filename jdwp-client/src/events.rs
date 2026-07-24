// JDWP event handling
//
// Events are sent from the JVM to notify about breakpoints, steps, etc.

use crate::commands::event_kinds;
use crate::protocol::JdwpResult;
use crate::reader::{read_i32, read_string, read_u64, read_u8};
use crate::types::{ThreadId, ReferenceTypeId, Location, ObjectId, FieldId, Value};
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
    MethodEntry {
        thread: ThreadId,
        location: Location,
    },
    MethodExit {
        thread: ThreadId,
        location: Location,
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
    Unknown {
        kind: u8,
    },
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
    ExceptionOnly {
        ref_type: ReferenceTypeId,
        caught: bool,
        uncaught: bool,
    },
    FieldOnly {
        ref_type: ReferenceTypeId,
        field_id: FieldId,
    },
    Step {
        thread: ThreadId,
        size: i32,
        depth: i32,
    },
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

        events.push(Event {
            kind,
            request_id,
            details,
        });
    }

    Ok(EventSet {
        suspend_policy,
        events,
    })
}

/// Dispatch a single event's kind-specific payload to the matching parser.
fn parse_event_details(kind: u8, buf: &mut &[u8]) -> JdwpResult<EventKind> {
    match kind {
        event_kinds::BREAKPOINT => parse_breakpoint_event(buf),
        event_kinds::SINGLE_STEP => parse_step_event(buf),
        event_kinds::VM_START => parse_vm_start_event(buf),
        event_kinds::VM_DEATH => Ok(EventKind::VMDeath),
        event_kinds::THREAD_START => parse_thread_start_event(buf),
        event_kinds::THREAD_DEATH => parse_thread_death_event(buf),
        event_kinds::CLASS_PREPARE => parse_class_prepare_event(buf),
        event_kinds::EXCEPTION => parse_exception_event(buf),
        event_kinds::FIELD_ACCESS => parse_field_access_event(buf),
        event_kinds::FIELD_MODIFICATION => parse_field_modification_event(buf),
        _ => {
            warn!("Unsupported event kind: {}", kind);
            Ok(EventKind::Unknown { kind })
        }
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
    let catch_location = if catch.class_id == 0 && catch.method_id == 0 && catch.index == 0 {
        None
    } else {
        Some(catch)
    };
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

/// Read a location from the buffer
fn read_location(buf: &mut &[u8]) -> JdwpResult<Location> {
    let type_tag = read_u8(buf)?;
    let class_id = read_u64(buf)?;
    let method_id = read_u64(buf)?;
    let index = read_u64(buf)?;

    Ok(Location {
        type_tag,
        class_id,
        method_id,
        index,
    })
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
    /// kinds we never requested (monitor events, frame pops), and dropping the set would lose
    /// the events beside it.
    #[test]
    fn an_unhandled_event_kind_becomes_unknown_rather_than_an_error() {
        // MONITOR_WAIT is a real JDWP kind this client never requests and does not parse — the honest
        // case, rather than a number the protocol doesn't define.
        let mut ev = vec![event_kinds::MONITOR_WAIT];
        ev.extend_from_slice(&1i32.to_be_bytes());
        let set = parse_event_packet(&packet(0, &[ev])).expect("an unhandled kind is not a parse failure");
        assert!(
            matches!(set.events.first().map(|e| &e.details), Some(EventKind::Unknown { kind })
                if *kind == event_kinds::MONITOR_WAIT),
            "expected Unknown, got {:?}",
            set.events.first().map(|e| &e.details)
        );
    }

    /// Every truncation of a valid packet must error rather than panic. This is the assertion that
    /// would have caught the pre-`reader` behaviour: a short `valueToBe` panicked the event-loop task,
    /// killing the whole debug session instead of reporting a malformed reply.
    #[test]
    fn every_truncation_of_a_packet_errors_instead_of_panicking() {
        for event in [breakpoint_event(5, 0xabc), field_modification_event(42)] {
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
