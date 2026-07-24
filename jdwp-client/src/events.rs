// JDWP event handling
//
// Events are sent from the JVM to notify about breakpoints, steps, etc.

use crate::commands::event_kinds;
use crate::protocol::JdwpResult;
use crate::reader::{read_i32, read_string, read_u64, read_u8};
use crate::types::{ThreadId, ReferenceTypeId, Location, ObjectId, FieldId};
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
    Unknown {
        kind: u8,
    },
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
