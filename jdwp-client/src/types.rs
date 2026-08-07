// JDWP type definitions
//
// Common types used across the JDWP protocol

use serde::{Deserialize, Serialize};

// Object IDs are 8 bytes in JDWP.
//
// SEVEN OF THESE ALIASES ARE UNREFERENCED, and they stay (CLEAN-2, #170; ADR-0044). They document JDWP's
// id space — that a thread group, a string, a class loader and an array are all `objectID` on the wire and
// are not interchangeable in the specification's intent. `#[allow(dead_code)]` per alias rather than on
// the file, because unlike `commands.rs` the rest of this module is live code where "unused" does mean
// something.
pub type ObjectId = u64;
pub type ThreadId = ObjectId;
#[allow(dead_code)]
pub(crate) type ThreadGroupId = ObjectId;
#[allow(dead_code)]
pub(crate) type StringId = ObjectId;
#[allow(dead_code)]
pub(crate) type ClassLoaderId = ObjectId;
#[allow(dead_code)]
pub(crate) type ClassObjectId = ObjectId;
#[allow(dead_code)]
pub(crate) type ArrayId = ObjectId;

pub type ReferenceTypeId = u64;
pub type ClassId = ReferenceTypeId;
#[allow(dead_code)]
pub(crate) type InterfaceId = ReferenceTypeId;
#[allow(dead_code)]
pub(crate) type ArrayTypeId = ReferenceTypeId;

pub type MethodId = u64;
pub type FieldId = u64;
pub type FrameId = u64;

// Location identifies a code position
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub(crate) type_tag: u8, // 1=class, 2=interface, 3=array
    pub class_id: ReferenceTypeId,
    pub method_id: MethodId,
    pub index: u64, // bytecode index (PC)
}

// Thread status values
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum ThreadStatus {
    Zombie = 0,
    Running = 1,
    Sleeping = 2,
    Monitor = 3,
    Wait = 4,
}

// Suspend status values
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum SuspendStatus {
    Running = 0,
    Suspended = 1,
}

// Type tags for values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TypeTag {
    Array = 91,        // '['
    Byte = 66,         // 'B'
    Char = 67,         // 'C'
    Object = 76,       // 'L'
    Float = 70,        // 'F'
    Double = 68,       // 'D'
    Int = 73,          // 'I'
    Long = 74,         // 'J'
    Short = 83,        // 'S'
    Void = 86,         // 'V'
    Boolean = 90,      // 'Z'
    String = 115,      // 's'
    Thread = 116,      // 't'
    ThreadGroup = 103, // 'g'
    ClassLoader = 108, // 'l'
    ClassObject = 99,  // 'c'
}

// Tagged value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Value {
    pub tag: u8,
    pub data: ValueData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ValueData {
    Byte(i8),
    Char(u16),
    Float(f32),
    Double(f64),
    Int(i32),
    Long(i64),
    Short(i16),
    Boolean(bool),
    Object(ObjectId),
    Void,
}

impl ValueData {
    /// Render a primitive wire value. `None` for a reference, which cannot be described without asking
    /// the debuggee about it.
    ///
    /// **One renderer, one home** (TYPE-1,
    /// [#48](https://github.com/YgorPerez/java-debugging-mcp/issues/48)). `mcp-server` carried a
    /// byte-identical copy of this match called `render_primitive`, and the copy was the one the tool
    /// actually ran — `Value::format` below was reached only through array elements and the
    /// type-mismatch message. That is what made this file's coverage number a lie: it measured 16.67%
    /// region and the review's verdict was "most arms are for types the probes never produce", which was
    /// half the answer. The other half was that the arms were *bypassed*. Rendering a wire value belongs
    /// to the crate that reads the wire, so the seam is here and `mcp-server` calls across it; the
    /// `Option` is what keeps the two audiences apart, because `mcp-server` has a much better answer for
    /// a reference than an id and is the only side that can pay a round trip to get it.
    #[must_use]
    pub fn format_primitive(&self) -> Option<String> {
        Some(match self {
            Self::Byte(v) => format!("(byte) {v}"),
            Self::Char(v) => format_char(*v),
            Self::Float(v) => format!("(float) {v}"),
            Self::Double(v) => format!("(double) {v}"),
            Self::Int(v) => format!("(int) {v}"),
            Self::Long(v) => format!("(long) {v}"),
            Self::Short(v) => format!("(short) {v}"),
            Self::Boolean(v) => format!("(boolean) {v}"),
            Self::Void => "(void)".to_string(),
            Self::Object(_) => return None,
        })
    }
}

/// Render one Java `char`, which is a UTF-16 **code unit** and not a Unicode scalar value.
///
/// The two are different sizes of thing, and half a surrogate pair is a perfectly ordinary value to find
/// in a `char` field or a `char[]` — a string sliced mid-pair leaves one behind, which is a real bug class
/// someone would reach for a debugger to chase. `char::from_u32` refuses exactly those code units, and
/// this used to answer `unwrap_or('?')`: `(char) 0xD800` came back as `(char) '?'`, byte for byte the same
/// as a genuine question mark, so the debugger hid the very thing it was being asked about (TYPE-1, #48).
///
/// Rendered as the `'\uD800'` escape Java itself would print, plus what it is, so the two readings can
/// never be confused again.
fn format_char(unit: u16) -> String {
    // `from_u32` fails only on the surrogate range, so `None` here IS "this is not a character".
    char::from_u32(u32::from(unit)).map_or_else(
        || format!("(char) '\\u{unit:04X}' (unpaired surrogate, not a character)"),
        |c| format!("(char) '{c}'"),
    )
}

impl Value {
    /// Format value for display.
    ///
    /// A thin shell over [`ValueData::format_primitive`] plus the one kind it declines: a reference, which
    /// this crate can only name by its id.
    #[must_use]
    pub fn format(&self) -> String {
        match &self.data {
            ValueData::Object(0) => "(object) null".to_string(),
            ValueData::Object(id) => format!("(object) @{id:x}"),
            primitive => primitive.format_primitive().unwrap_or_default(),
        }
    }
}

// Variable information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub code_index: u64,
    pub name: String,
    pub signature: String,
    /// The **generic** signature from the class file's `Signature` attribute, when it carries one
    /// (DISC-12, #95).
    ///
    /// `None` is the ordinary answer, not a degraded one: the attribute is optional, absent for code
    /// compiled without it and for synthetic members whose types were erased. JDWP's generic commands
    /// answer with an EMPTY STRING in that case rather than an error, and an empty string is normalised to
    /// `None` here so that no caller can render a blank type.
    pub generic_signature: Option<String>,
    pub length: u32,
    pub slot: u32,
}

// Stack frame information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameInfo {
    pub(crate) frame_id: FrameId,
    pub(crate) location: Location,
}

#[cfg(test)]
mod tests {
    use super::{Value, ValueData};

    /// TYPE-1 (#48): `(char) 0xD800` is half a surrogate pair — an ordinary thing to find in a Java
    /// `char[]`, since a `char` is a UTF-16 code unit and not a Unicode scalar value — and it used to
    /// render as `(char) '?'`, byte for byte a real question mark.
    ///
    /// The comparison against a genuine `'?'` is the whole test. "Renders as something" was never the
    /// missing property; "renders as something a caller can tell apart from a real value" was.
    #[test]
    fn an_unpaired_surrogate_is_rendered_apart_from_a_real_question_mark() {
        let surrogate = ValueData::Char(0xD800).format_primitive().expect("a char is a primitive");
        let question = ValueData::Char(u16::from(b'?')).format_primitive().expect("so is this one");

        assert_eq!(question, "(char) '?'", "a real question mark still renders as itself");
        assert_ne!(surrogate, question, "the two must not be the same bytes: {surrogate}");
        assert!(surrogate.contains("\\uD800"), "the code unit itself is shown: {surrogate}");
        assert!(surrogate.contains("unpaired surrogate"), "and what it is: {surrogate}");

        // The range is 0xD800..=0xDFFF, not one code unit, and the low half is what a string sliced
        // mid-pair actually leaves behind.
        let low = ValueData::Char(0xDFFF).format_primitive().expect("still a char");
        assert!(low.contains("\\uDFFF"), "the low half of the range is covered too: {low}");
        assert!(low.contains("unpaired surrogate"), "{low}");

        // Everything either side of the range is a character and renders as one.
        assert_eq!(ValueData::Char(0xD7FF).format_primitive().unwrap(), "(char) '\u{d7ff}'");
        assert_eq!(ValueData::Char(0xE000).format_primitive().unwrap(), "(char) '\u{e000}'");
    }

    /// One renderer, one home (TYPE-1, #48). `mcp-server` used to carry a byte-identical copy of the
    /// primitive match and call *that*, so this crate's copy was cold and its coverage number said
    /// nothing. Both sides now cross the same seam, and the only thing that differs is what each does
    /// with the reference `format_primitive` declines.
    #[test]
    fn only_a_reference_declines_to_render_without_asking_the_debuggee() {
        assert_eq!(ValueData::Int(-2_147_483_648).format_primitive().unwrap(), "(int) -2147483648");
        assert_eq!(ValueData::Boolean(true).format_primitive().unwrap(), "(boolean) true");
        assert_eq!(ValueData::Byte(-7).format_primitive().unwrap(), "(byte) -7");
        assert_eq!(ValueData::Short(-300).format_primitive().unwrap(), "(short) -300");
        assert_eq!(ValueData::Long(9_000_000_000).format_primitive().unwrap(), "(long) 9000000000");
        assert_eq!(ValueData::Float(1.5).format_primitive().unwrap(), "(float) 1.5");
        assert_eq!(ValueData::Double(-2.25).format_primitive().unwrap(), "(double) -2.25");

        assert!(
            ValueData::Object(0x2b).format_primitive().is_none(),
            "a reference needs a round trip to describe, and this crate is not the side that pays it"
        );
        // `Value::format` is the shell that answers anyway, with the id it can see from here.
        assert_eq!(Value { tag: 76, data: ValueData::Object(0x2b) }.format(), "(object) @2b");
        assert_eq!(Value { tag: 76, data: ValueData::Object(0) }.format(), "(object) null");
        // …and defers to the same renderer for everything else, so the two can never drift apart again.
        assert_eq!(Value { tag: 67, data: ValueData::Char(0xD800) }.format(), surrogate_rendering());
    }

    fn surrogate_rendering() -> String {
        ValueData::Char(0xD800).format_primitive().expect("a char is a primitive")
    }
}
