// Helper functions for reading JDWP data types from buffers
//
// Every reader here checks the buffer before it reads. `bytes::Buf::get_*` PANICS on a short buffer,
// and these run inside the event-loop task: a panic there kills the connection instead of surfacing
// as an error the caller can report. A truncated or malformed reply is not hypothetical — it is what
// a version-skewed JVM, a half-closed socket, or a bug in our own request framing produces.
//
// # Every JDWP id is read as 8 bytes, and that is ASSUMED — nothing checks it
//
// `objectID`, `referenceTypeID`, `methodID`, `fieldID` and `frameID` are all read with [`read_u64`],
// and [`value_width`] gives every reference tag a width of 8. The JDWP spec does not fix those widths:
// a VM declares them in `VirtualMachine.IDSizes`, and this crate never asks. It holds on every 64-bit
// `HotSpot`, which is what this tool attaches to.
//
// The assumption is deliberate and **unvalidated**. A wrapper for `IDSizes` existed and was deleted by
// CLEAN-1 (#27) precisely because it was never called: an uncalled command made the widths look
// checked. On a VM that reported narrower ids, every read after the first id would be misaligned and
// the failure would surface as garbled values or an `Unknown value tag`, not as a clear mismatch. If
// that ever needs guarding, the fix is a real check at attach time that refuses the session — not a
// function nobody calls.

use crate::protocol::{JdwpError, JdwpResult};
use crate::types::ValueData;
use bytes::Buf;

/// JDWP value tags (JDWP spec, `Value` / `TaggedObjectID`). Each is the ASCII code of the JNI type
/// signature character, which is why they look arbitrary as numbers.
pub mod value_tags {
    pub const BYTE: u8 = 66; // 'B'
    pub const CHAR: u8 = 67; // 'C'
    pub const OBJECT: u8 = 76; // 'L'
    pub const FLOAT: u8 = 70; // 'F'
    pub const DOUBLE: u8 = 68; // 'D'
    pub const INT: u8 = 73; // 'I'
    pub const LONG: u8 = 74; // 'J'
    pub const SHORT: u8 = 83; // 'S'
    pub const VOID: u8 = 86; // 'V'
    pub const BOOLEAN: u8 = 90; // 'Z'
    pub const STRING: u8 = 115; // 's'
    pub const THREAD: u8 = 116; // 't'
    pub const THREAD_GROUP: u8 = 103; // 'g'
    pub const CLASS_LOADER: u8 = 108; // 'l'
    pub const CLASS_OBJECT: u8 = 99; // 'c'
    pub const ARRAY: u8 = 91; // '['
}

/// Error unless `buf` holds at least `n` more bytes. `what` names the thing being read, so a
/// truncated reply says which field ran out rather than just that something did.
fn ensure(buf: &[u8], n: usize, what: &str) -> JdwpResult<()> {
    if buf.remaining() < n {
        return Err(JdwpError::Protocol(format!(
            "Not enough data for {what}: need {n} byte(s), have {}",
            buf.remaining()
        )));
    }
    Ok(())
}

/// How many bytes a value of this tag occupies, or `Err` for a tag we don't know.
///
/// Resolving the width up front is what makes [`read_value_by_tag`] total: an unknown tag fails
/// before the buffer is touched, and a known one is bounds-checked once instead of per branch.
fn value_width(tag: u8) -> JdwpResult<usize> {
    use value_tags as t;
    Ok(match tag {
        t::VOID => 0,
        t::BYTE | t::BOOLEAN => 1,
        t::CHAR | t::SHORT => 2,
        t::FLOAT | t::INT => 4,
        // 8 covers both the 64-bit primitives and every reference kind, which is an objectID.
        t::DOUBLE
        | t::LONG
        | t::OBJECT
        | t::STRING
        | t::THREAD
        | t::THREAD_GROUP
        | t::CLASS_LOADER
        | t::CLASS_OBJECT
        | t::ARRAY => 8,
        _ => return Err(JdwpError::Protocol(format!("Unknown value tag: {tag}"))),
    })
}

/// Read an untagged value whose type is named by a JDWP value `tag`.
///
/// The one implementation of this: it used to be copied into `eval`, `stackframe` and `object`, all
/// three of which read the buffer without checking it first.
///
/// # Errors
/// Returns a [`JdwpError`] for an unknown tag, or if the buffer is too short for the value.
pub fn read_value_by_tag(tag: u8, buf: &mut &[u8]) -> JdwpResult<ValueData> {
    use value_tags as t;
    let width = value_width(tag)?;
    ensure(buf, width, "value")?;
    // Checked above, so every `get_*` below is infallible.
    Ok(match tag {
        t::BYTE => ValueData::Byte(buf.get_i8()),
        t::CHAR => ValueData::Char(buf.get_u16()),
        t::DOUBLE => ValueData::Double(buf.get_f64()),
        t::FLOAT => ValueData::Float(buf.get_f32()),
        t::INT => ValueData::Int(buf.get_i32()),
        t::LONG => ValueData::Long(buf.get_i64()),
        t::SHORT => ValueData::Short(buf.get_i16()),
        t::BOOLEAN => ValueData::Boolean(buf.get_u8() != 0),
        t::VOID => ValueData::Void,
        _ => ValueData::Object(buf.get_u64()),
    })
}

/// Read a JDWP string (4-byte length prefix + UTF-8 bytes)
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_string(buf: &mut &[u8]) -> JdwpResult<String> {
    ensure(buf, 4, "string length")?;
    let len = buf.get_u32() as usize;
    // The length comes off the wire, so a corrupt one can claim gigabytes. Checking it against what is
    // actually left is the whole defence; `ensure` first so the error names the shortfall, then a
    // checked slice so this can't panic even if the two ever disagreed.
    ensure(buf, len, "string body")?;
    let bytes = buf
        .get(..len)
        .ok_or_else(|| JdwpError::Protocol("String shorter than its length prefix".to_string()))?
        .to_vec();
    buf.advance(len);

    String::from_utf8(bytes).map_err(|e| JdwpError::Protocol(format!("Invalid UTF-8 in string: {e}")))
}

/// Read a u32
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_u32(buf: &mut &[u8]) -> JdwpResult<u32> {
    ensure(buf, 4, "u32")?;
    Ok(buf.get_u32())
}

/// Read a i32
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_i32(buf: &mut &[u8]) -> JdwpResult<i32> {
    ensure(buf, 4, "i32")?;
    Ok(buf.get_i32())
}

/// Read a u8
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_u8(buf: &mut &[u8]) -> JdwpResult<u8> {
    ensure(buf, 1, "u8")?;
    Ok(buf.get_u8())
}

/// Read a u64
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_u64(buf: &mut &[u8]) -> JdwpResult<u64> {
    ensure(buf, 8, "u64")?;
    Ok(buf.get_u64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::write_untagged_value;
    use crate::types::Value;

    /// One value of every `ValueData` variant, with its tag. `Void` has no bytes, so it is exercised
    /// separately rather than round-tripped.
    fn sample_values() -> Vec<(u8, ValueData)> {
        use value_tags as t;
        vec![
            (t::BYTE, ValueData::Byte(-7)),
            (t::CHAR, ValueData::Char(u16::from(b'Q'))),
            (t::SHORT, ValueData::Short(-300)),
            (t::INT, ValueData::Int(i32::MIN)),
            (t::LONG, ValueData::Long(i64::MAX)),
            (t::FLOAT, ValueData::Float(1.5)),
            (t::DOUBLE, ValueData::Double(-2.25)),
            (t::BOOLEAN, ValueData::Boolean(true)),
            (t::OBJECT, ValueData::Object(0xdead_beef)),
        ]
    }

    /// The writer and the reader have to agree byte for byte — they are the two halves of every
    /// `SetValues` / `GetValues` round trip, and a mismatch would show up as a plausible wrong number
    /// rather than an error.
    #[test]
    fn every_value_variant_round_trips_through_write_then_read() {
        let mut bytes = Vec::new();
        for (tag, data) in sample_values() {
            bytes.clear();
            write_untagged_value(&mut bytes, &Value { tag, data: data.clone() });
            assert_eq!(
                bytes.len(),
                value_width(tag).expect("sample tags are all known"),
                "tag {tag} wrote {} bytes, width table says otherwise",
                bytes.len()
            );

            let mut buf = bytes.as_slice();
            let read = read_value_by_tag(tag, &mut buf).expect("round trip");
            assert_eq!(format!("{read:?}"), format!("{data:?}"), "tag {tag} changed on the way back");
            assert!(buf.is_empty(), "tag {tag} left {} unread byte(s)", buf.len());
        }
    }

    /// A short buffer must be an error, never a panic: these run in the event-loop task, where a panic
    /// takes the connection down instead of being reported.
    #[test]
    fn a_truncated_buffer_errors_for_every_value_tag() {
        let mut full = Vec::new();
        for (tag, data) in sample_values() {
            full.clear();
            write_untagged_value(&mut full, &Value { tag, data });
            // Every length short of complete, including empty.
            for keep in 0..full.len() {
                let mut buf = &full[..keep];
                let err = read_value_by_tag(tag, &mut buf);
                assert!(
                    err.is_err(),
                    "tag {tag} accepted {keep} of {} byte(s) instead of erroring",
                    full.len()
                );
            }
        }
    }

    #[test]
    fn void_reads_from_an_empty_buffer_and_consumes_nothing() {
        let mut buf: &[u8] = &[];
        assert!(matches!(read_value_by_tag(value_tags::VOID, &mut buf), Ok(ValueData::Void)));
        assert!(buf.is_empty());
    }

    /// An unrecognised tag is refused before the buffer is touched, so a bogus tag can't be read as
    /// whatever happens to follow it.
    #[test]
    fn an_unknown_value_tag_errors_without_consuming_input() {
        let mut buf: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8];
        let err = read_value_by_tag(b'?', &mut buf).expect_err("'?' is not a value tag");
        assert!(format!("{err}").contains("Unknown value tag"), "unhelpful error: {err}");
        assert_eq!(buf.len(), 8, "a rejected tag must not advance the buffer");
    }

    #[test]
    fn each_fixed_width_reader_errors_on_a_short_buffer() {
        assert!(read_u8(&mut &[][..]).is_err());
        assert!(read_u32(&mut &[0u8; 3][..]).is_err());
        assert!(read_i32(&mut &[0u8; 3][..]).is_err());
        assert!(read_u64(&mut &[0u8; 7][..]).is_err());
        // And each succeeds at exactly its width, so the checks aren't off by one.
        assert_eq!(read_u8(&mut &[9u8][..]).expect("u8"), 9);
        assert_eq!(read_u32(&mut &[0, 0, 1, 0][..]).expect("u32"), 256);
        assert_eq!(read_i32(&mut &[0xff, 0xff, 0xff, 0xff][..]).expect("i32"), -1);
        assert_eq!(read_u64(&mut &[0, 0, 0, 0, 0, 0, 0, 5][..]).expect("u64"), 5);
    }

    #[test]
    fn read_string_handles_empty_truncated_and_lying_lengths() {
        // A well-formed string, and the buffer left positioned after it.
        let mut wire = 5u32.to_be_bytes().to_vec();
        wire.extend_from_slice(b"hello");
        wire.extend_from_slice(b"tail");
        let mut buf = wire.as_slice();
        assert_eq!(read_string(&mut buf).expect("string"), "hello");
        assert_eq!(buf, b"tail", "read_string must consume exactly the string");

        // Empty string: a length of 0 is legal, not a truncation.
        assert_eq!(read_string(&mut &0u32.to_be_bytes()[..]).expect("empty"), "");

        // Truncated length prefix, and a length that overruns the body.
        assert!(read_string(&mut &[0u8, 0, 5][..]).is_err());
        let mut lying = 99u32.to_be_bytes().to_vec();
        lying.extend_from_slice(b"short");
        assert!(read_string(&mut lying.as_slice()).is_err(), "a length past the end must error");

        // Invalid UTF-8 is an error, not a lossy string: a mangled class signature would be worse
        // than a reported failure.
        let mut bad = 2u32.to_be_bytes().to_vec();
        bad.extend_from_slice(&[0xff, 0xfe]);
        assert!(read_string(&mut bad.as_slice()).is_err());
    }
}
