//! JDWP command implementations — the specification's numbers, transcribed.
//!
//! **`dead_code` is allowed for this whole file, and that is a decision rather than a silencing**
//! (CLEAN-2, #170; ADR-0044). Every item here is `pub(crate)` now, and 27 of them are not sent by this
//! crate — so rustc reports them unused. "The binary does not send `VirtualMachine.HoldEvents`" is not
//! "`HoldEvents` is not part of JDWP": these are JDWP's facts, not ours. We did not choose the numbers,
//! we cannot break them, and they cannot rot. ADR-0044 rejected deleting them for exactly that reason —
//! it "buys a smaller file and loses a table".
//!
//! The cost is real and worth stating: a genuinely dead constant added here in future will not be
//! reported either. That is acceptable because *unused* carries no signal in a transcription, which is
//! the whole argument above — and it is why this allow stops at this file rather than sitting on the
//! crate.
#![allow(dead_code)]

// Command Sets:
// 1 = VirtualMachine
// 2 = ReferenceType
// 6 = Method
// 9 = ObjectReference
// 11 = ThreadReference
// 15 = EventRequest
// 16 = StackFrame

// Command set IDs
pub mod command_sets {
    pub const VIRTUAL_MACHINE: u8 = 1;
    pub const REFERENCE_TYPE: u8 = 2;
    pub(crate) const CLASS_TYPE: u8 = 3;
    pub const METHOD: u8 = 6;
    pub const OBJECT_REFERENCE: u8 = 9;
    pub(crate) const STRING_REFERENCE: u8 = 10;
    pub(crate) const THREAD_REFERENCE: u8 = 11;
    pub(crate) const THREAD_GROUP_REFERENCE: u8 = 12;
    pub(crate) const ARRAY_REFERENCE: u8 = 13;
    pub const EVENT_REQUEST: u8 = 15;
    pub(crate) const STACK_FRAME: u8 = 16;
}

// VirtualMachine commands (set 1)
pub mod vm_commands {
    pub(crate) const VERSION: u8 = 1;
    pub(crate) const CLASSES_BY_SIGNATURE: u8 = 2;
    pub(crate) const ALL_CLASSES: u8 = 3;
    pub(crate) const ALL_THREADS: u8 = 4;
    pub(crate) const TOP_LEVEL_THREAD_GROUPS: u8 = 5;
    pub(crate) const DISPOSE: u8 = 6;
    pub(crate) const ID_SIZES: u8 = 7;
    pub(crate) const SUSPEND: u8 = 8;
    pub(crate) const RESUME: u8 = 9;
    pub(crate) const EXIT: u8 = 10;
    pub(crate) const CREATE_STRING: u8 = 11;
    pub(crate) const CAPABILITIES: u8 = 12;
    pub(crate) const CLASS_PATHS: u8 = 13;
    pub(crate) const DISPOSE_OBJECTS: u8 = 14;
    pub(crate) const HOLD_EVENTS: u8 = 15;
    pub(crate) const RELEASE_EVENTS: u8 = 16;
    pub const CAPABILITIES_NEW: u8 = 17;
    pub(crate) const REDEFINE_CLASSES: u8 = 18;
    /// How many live instances each of several types has — one heap walk for the whole batch, which is
    /// why the tool takes a list (DISC-10).
    pub(crate) const INSTANCE_COUNTS: u8 = 21;
}

// ReferenceType commands (set 2)
pub mod reference_type_commands {
    pub(crate) const SIGNATURE: u8 = 1;
    pub(crate) const CLASS_LOADER: u8 = 2;
    pub(crate) const MODIFIERS: u8 = 3;
    pub(crate) const FIELDS: u8 = 4;
    pub(crate) const METHODS: u8 = 5;
    pub(crate) const GET_VALUES: u8 = 6;
    pub(crate) const SOURCE_FILE: u8 = 7;
    pub(crate) const NESTED_TYPES: u8 = 8;
    pub(crate) const STATUS: u8 = 9;
    pub(crate) const INTERFACES: u8 = 10;
    pub(crate) const CLASS_OBJECT: u8 = 11;
    pub(crate) const SOURCE_DEBUG_EXTENSION: u8 = 12;
    pub(crate) const SIGNATURE_WITH_GENERIC: u8 = 13;
    pub(crate) const FIELDS_WITH_GENERIC: u8 = 14;
    pub(crate) const METHODS_WITH_GENERIC: u8 = 15;
    /// The live instances of ONE type — **exact type, not subtype-inclusive** (DISC-10).
    pub(crate) const INSTANCES: u8 = 16;
}

// Method commands (set 6)
pub mod method_commands {
    pub(crate) const LINE_TABLE: u8 = 1;
    pub(crate) const VARIABLE_TABLE: u8 = 2;
    pub(crate) const BYTECODES: u8 = 3;
    pub const IS_OBSOLETE: u8 = 4;
    pub(crate) const VARIABLE_TABLE_WITH_GENERIC: u8 = 5;
}

// ThreadReference commands (set 11)
pub mod thread_commands {
    pub(crate) const NAME: u8 = 1;
    pub(crate) const SUSPEND: u8 = 2;
    pub(crate) const RESUME: u8 = 3;
    pub(crate) const STATUS: u8 = 4;
    pub(crate) const THREAD_GROUP: u8 = 5;
    pub(crate) const FRAMES: u8 = 6;
    pub(crate) const FRAME_COUNT: u8 = 7;
    pub(crate) const OWNED_MONITORS: u8 = 8;
    pub(crate) const CURRENT_CONTENDED_MONITOR: u8 = 9;
    pub(crate) const STOP: u8 = 10;
    pub(crate) const INTERRUPT: u8 = 11;
    pub(crate) const SUSPEND_COUNT: u8 = 12;
    pub(crate) const FORCE_EARLY_RETURN: u8 = 14;
}

// EventRequest commands (set 15)
pub mod event_commands {
    pub const SET: u8 = 1;
    pub const CLEAR: u8 = 2;
    pub(crate) const CLEAR_ALL_BREAKPOINTS: u8 = 3;
}

// StringReference commands (set 10)
pub mod string_reference_commands {
    pub(crate) const VALUE: u8 = 1;
}

// ObjectReference commands (set 9)
pub mod object_reference_commands {
    pub(crate) const REFERENCE_TYPE: u8 = 1;
    pub(crate) const GET_VALUES: u8 = 2;
    pub(crate) const SET_VALUES: u8 = 3;
    pub(crate) const MONITOR_INFO: u8 = 5;
    pub(crate) const INVOKE_METHOD: u8 = 6;
    pub(crate) const DISABLE_COLLECTION: u8 = 7;
    pub(crate) const ENABLE_COLLECTION: u8 = 8;
    pub(crate) const IS_COLLECTED: u8 = 9;
}

// StackFrame commands (set 16)
pub mod stack_frame_commands {
    pub(crate) const GET_VALUES: u8 = 1;
    pub(crate) const SET_VALUES: u8 = 2;
    pub(crate) const THIS_OBJECT: u8 = 3;
    pub(crate) const POP_FRAMES: u8 = 4;
}

// Event kinds for EventRequest.Set.
//
// This is the protocol's full table, so most entries are named but unrequested — that is deliberate, and
// different from having decode paths for an event nothing can arm. Two worth knowing about:
//
// - `METHOD_ENTRY` (40) is named but intentionally NOT wired up. With a `ClassMatch` it fires on every
//   method of every matching class — the noisiest event in JDWP — and "what calls this?" is answered
//   far more cheaply by a traced breakpoint's caller chain (TRACE-5). `EventKind::MethodEntry` was
//   removed for that reason (METH-1); this constant is a spec reference, not an oversight to fix.
// - The `MONITOR_*` kinds (43-46) are the event-driven view of lock contention. The polling view —
//   `owned_monitors` / `current_contended_monitor`, behind `debug.thread_dump` — is what DUMP-1 needed;
//   these would only be worth arming for a live contention *detector*.
pub mod event_kinds {
    pub(crate) const SINGLE_STEP: u8 = 1;
    pub(crate) const BREAKPOINT: u8 = 2;
    pub(crate) const FRAME_POP: u8 = 3;
    pub(crate) const EXCEPTION: u8 = 4;
    pub(crate) const USER_DEFINED: u8 = 5;
    pub(crate) const THREAD_START: u8 = 6;
    pub(crate) const THREAD_DEATH: u8 = 7;
    pub(crate) const CLASS_PREPARE: u8 = 8;
    pub(crate) const CLASS_UNLOAD: u8 = 9;
    pub(crate) const CLASS_LOAD: u8 = 10;
    pub(crate) const FIELD_ACCESS: u8 = 20;
    pub(crate) const FIELD_MODIFICATION: u8 = 21;
    pub(crate) const EXCEPTION_CATCH: u8 = 30;
    pub(crate) const METHOD_ENTRY: u8 = 40;
    pub(crate) const METHOD_EXIT: u8 = 41;
    pub(crate) const METHOD_EXIT_WITH_RETURN_VALUE: u8 = 42;
    pub(crate) const MONITOR_CONTENDED_ENTER: u8 = 43;
    pub(crate) const MONITOR_CONTENDED_ENTERED: u8 = 44;
    pub(crate) const MONITOR_WAIT: u8 = 45;
    pub(crate) const MONITOR_WAITED: u8 = 46;
    pub(crate) const VM_START: u8 = 90;
    pub(crate) const VM_DEATH: u8 = 99;
}

// Step sizes
pub mod step_sizes {
    pub(crate) const MIN: i32 = 0;
    pub(crate) const LINE: i32 = 1;
}

// Step depths
pub mod step_depths {
    pub(crate) const INTO: i32 = 0;
    pub(crate) const OVER: i32 = 1;
    pub(crate) const OUT: i32 = 2;
}
