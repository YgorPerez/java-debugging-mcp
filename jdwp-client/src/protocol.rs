// JDWP protocol definitions and packet handling
//
// Reference: https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html

use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;

// JDWP uses big-endian (network byte order) for all multi-byte values
// This is architecture-independent (works on Intel, ARM M1/M2/M3, etc.)

pub type JdwpResult<T> = Result<T, JdwpError>;

#[derive(Debug, Error)]
pub enum JdwpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Invalid handshake")]
    InvalidHandshake,

    #[error("JDWP error code {0}: {1}")]
    JdwpErrorCode(u16, String),

    /// The connection to the debuggee ended, carrying **why** it ended.
    ///
    /// The payload is the point. Every one of these was once reported as `Reply channel closed`, a
    /// message produced by four different worlds — a dead socket, a dropped event consumer, a lapsed
    /// reply, and a loop that had already gone — none of which the reader could tell apart. The event
    /// loop is the only thing that knows which, and it used to log the cause at a level nobody enables
    /// and then throw the value away, so a debuggee that died mid-question was indistinguishable from a
    /// bug in this crate. See [`crate::eventloop`].
    #[error("connection to the debuggee closed: {0}")]
    ConnectionClosed(String),

    /// A command was sent, the connection stayed up, and no reply arrived within the budget.
    ///
    /// Distinct from [`ConnectionClosed`](Self::ConnectionClosed) because the remedy differs: the socket
    /// is still there, so the session is worth keeping and the *question* is what failed. Distinct from
    /// [`InvokeTimeout`](Self::InvokeTimeout) because nothing was being executed in the debuggee — this
    /// is the JVM not answering a question it should have answered.
    #[error(
        "no reply from the debuggee within {0}s (the connection is still open; the command was abandoned)"
    )]
    ReplyTimeout(u64),

    /// A debuggee invocation did not return within its budget.
    ///
    /// Distinct from a lost reply on purpose. `INVOKE_SINGLE_THREADED` runs only the target thread, so a
    /// method needing a monitor held by one of the *other* (still suspended) threads cannot finish — the
    /// classic debugger-invocation deadlock. That is not a protocol failure and not something the caller
    /// did wrong, and it must be reported as itself rather than folded into a generic error, because the
    /// right response is different: render shallowly and move on.
    #[error("invocation did not return within {0}ms (the debuggee thread may be blocked on a monitor held by another suspended thread)")]
    InvokeTimeout(u64),

    /// The connection is in read-only mode and something tried to execute code in the debuggee.
    ///
    /// Enforced at the point of invocation rather than by inspecting expressions up in the MCP layer,
    /// because invocation is reached from many directions — a `toString()` render, a `List.get`
    /// subscript, `valueOf` boxing, a breakpoint condition — and a text-level guard misses whichever
    /// one nobody thought of.
    #[error("read-only connection: refusing {0} in the debuggee")]
    ReadOnly(String),
}

// JDWP handshake string
pub const JDWP_HANDSHAKE: &[u8] = b"JDWP-Handshake";

// Packet structure:
// length (4 bytes) - includes header
// id (4 bytes)
// flags (1 byte) - 0x00 = command, 0x80 = reply
// [Command packet: command set (1 byte) + command (1 byte)]
// [Reply packet: error code (2 bytes)]
// data (variable)

pub const HEADER_SIZE: usize = 11;
pub const REPLY_FLAG: u8 = 0x80;

#[derive(Debug, Clone)]
pub struct CommandPacket {
    pub id: u32,
    pub command_set: u8,
    pub command: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ReplyPacket {
    pub id: u32,
    pub error_code: u16,
    pub data: Vec<u8>,
}

impl CommandPacket {
    #[must_use]
    pub const fn new(id: u32, command_set: u8, command: u8) -> Self {
        Self { id, command_set, command, data: Vec::new() }
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let length = HEADER_SIZE + self.data.len();
        let mut buf = BytesMut::with_capacity(length);

        buf.put_u32(u32::try_from(length).unwrap_or(u32::MAX));
        buf.put_u32(self.id);
        buf.put_u8(0x00); // command flag
        buf.put_u8(self.command_set);
        buf.put_u8(self.command);
        buf.put_slice(&self.data);

        buf.to_vec()
    }
}

/// `ABSENT_INFORMATION` — the class is there, the debug attribute being asked for is not (`javac
/// -g:none`, a synthetic class, a JVM without an optional table).
///
/// Public because callers *branch* on this one instead of reporting it. It is a fact about how the
/// class was compiled, and telling it apart from a transport failure is the difference between "this
/// build has no line numbers" and "the debugger is broken".
pub const ERR_ABSENT_INFORMATION: u16 = 101;

/// `NOT_IMPLEMENTED` — the VM never had the optional capability behind the command. Public for the
/// same reason as [`ERR_ABSENT_INFORMATION`]: for an optional command it is an answer, not a fault.
pub const ERR_NOT_IMPLEMENTED: u16 = 99;

/// JDWP error-code to human-readable name mapping.
const ERROR_MESSAGES: &[(u16, &str)] = &[
    (0, "NONE"),
    (10, "INVALID_THREAD"),
    (11, "INVALID_THREAD_GROUP"),
    (12, "INVALID_PRIORITY"),
    (13, "THREAD_NOT_SUSPENDED"),
    (14, "THREAD_SUSPENDED"),
    (20, "INVALID_OBJECT"),
    (21, "INVALID_CLASS"),
    (22, "CLASS_NOT_PREPARED"),
    (23, "INVALID_METHODID"),
    (24, "INVALID_LOCATION"),
    (25, "INVALID_FIELDID"),
    (30, "INVALID_FRAMEID"),
    (31, "NO_MORE_FRAMES"),
    (32, "OPAQUE_FRAME"),
    (33, "NOT_CURRENT_FRAME"),
    (34, "TYPE_MISMATCH"),
    (35, "INVALID_SLOT"),
    (40, "DUPLICATE"),
    (41, "NOT_FOUND"),
    (50, "INVALID_MONITOR"),
    (51, "NOT_MONITOR_OWNER"),
    (52, "INTERRUPT"),
    (60, "INVALID_CLASS_FORMAT"),
    (61, "CIRCULAR_CLASS_DEFINITION"),
    (62, "FAILS_VERIFICATION"),
    (63, "ADD_METHOD_NOT_IMPLEMENTED"),
    (64, "SCHEMA_CHANGE_NOT_IMPLEMENTED"),
    (65, "INVALID_TYPESTATE"),
    (66, "HIERARCHY_CHANGE_NOT_IMPLEMENTED"),
    (67, "DELETE_METHOD_NOT_IMPLEMENTED"),
    (68, "UNSUPPORTED_VERSION"),
    (69, "NAMES_DONT_MATCH"),
    (70, "CLASS_MODIFIERS_CHANGE_NOT_IMPLEMENTED"),
    (71, "METHOD_MODIFIERS_CHANGE_NOT_IMPLEMENTED"),
    (99, "NOT_IMPLEMENTED"),
    (100, "NULL_POINTER"),
    (101, "ABSENT_INFORMATION"),
    (102, "INVALID_EVENT_TYPE"),
    (103, "ILLEGAL_ARGUMENT"),
    (110, "OUT_OF_MEMORY"),
    (111, "ACCESS_DENIED"),
    (112, "VM_DEAD"),
    (113, "INTERNAL"),
    (115, "UNATTACHED_THREAD"),
    (500, "INVALID_TAG"),
    (502, "ALREADY_INVOKING"),
    (503, "INVALID_INDEX"),
    (504, "INVALID_LENGTH"),
    (506, "INVALID_STRING"),
    (507, "INVALID_CLASS_LOADER"),
    (508, "INVALID_ARRAY"),
    (509, "TRANSPORT_LOAD"),
    (510, "TRANSPORT_INIT"),
    (511, "NATIVE_METHOD"),
    (512, "INVALID_COUNT"),
];

impl ReplyPacket {
    /// Decode a reply packet from its raw bytes.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the buffer is too short or the reply flag is invalid.
    pub fn decode(mut buf: &[u8]) -> JdwpResult<Self> {
        if buf.len() < HEADER_SIZE {
            return Err(JdwpError::Protocol("Reply packet too short".to_string()));
        }

        let _length = buf.get_u32();
        let id = buf.get_u32();
        let flags = buf.get_u8();

        if flags != REPLY_FLAG {
            return Err(JdwpError::Protocol(format!("Invalid reply flag: {flags:#x}")));
        }

        let error_code = buf.get_u16();
        let data = buf.to_vec();

        Ok(Self { id, error_code, data })
    }

    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.error_code != 0
    }

    /// Return an error if the reply carries a non-zero JDWP error code.
    ///
    /// # Errors
    /// Returns a [`JdwpError::JdwpErrorCode`] when the reply's error code is non-zero.
    pub fn check_error(&self) -> JdwpResult<()> {
        if self.is_error() {
            Err(JdwpError::JdwpErrorCode(self.error_code, self.error_message().to_string()))
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    #[must_use]
    pub fn error_message(&self) -> &'static str {
        ERROR_MESSAGES
            .iter()
            .find(|&&(code, _)| code == self.error_code)
            .map_or("UNKNOWN_ERROR", |&(_, name)| name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_packet_encode() {
        let packet = CommandPacket::new(1, 1, 1);
        let encoded = packet.encode();

        assert_eq!(encoded.len(), HEADER_SIZE);
        assert_eq!(&encoded[0..4], &[0, 0, 0, 11]); // length (big-endian)
        assert_eq!(&encoded[4..8], &[0, 0, 0, 1]); // id (big-endian)
        assert_eq!(encoded[8], 0x00); // command flag
        assert_eq!(encoded[9], 1); // command set
        assert_eq!(encoded[10], 1); // command
    }

    #[test]
    fn test_big_endian_encoding() {
        // Verify we're using big-endian (network byte order)
        // This test ensures architecture independence (Intel vs ARM M1/M2/M3)
        let packet = CommandPacket::new(0x1234_5678, 1, 1);
        let encoded = packet.encode();

        // ID should be encoded as big-endian: 0x12345678
        assert_eq!(&encoded[4..8], &[0x12, 0x34, 0x56, 0x78]);

        // NOT little-endian (which would be [0x78, 0x56, 0x34, 0x12])
        assert_ne!(&encoded[4..8], &[0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn test_reply_packet_decode() {
        // Construct a reply packet manually with big-endian values
        let reply_data = vec![
            0, 0, 0, 11, // length = 11 (big-endian)
            0, 0, 0, 1,    // id = 1 (big-endian)
            0x80, // reply flag
            0, 0, // error code = 0 (big-endian)
        ];

        let packet = ReplyPacket::decode(&reply_data).unwrap();
        assert_eq!(packet.id, 1);
        assert_eq!(packet.error_code, 0);
        assert!(!packet.is_error());
    }
}
