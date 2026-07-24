// Helper functions for reading JDWP data types from buffers

use bytes::Buf;
use crate::protocol::{JdwpError, JdwpResult};

/// Read a JDWP string (4-byte length prefix + UTF-8 bytes)
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_string(buf: &mut &[u8]) -> JdwpResult<String> {
    if buf.remaining() < 4 {
        return Err(JdwpError::Protocol("Not enough data for string length".to_string()));
    }

    let len = buf.get_u32() as usize;

    if buf.remaining() < len {
        return Err(JdwpError::Protocol(format!(
            "Not enough data for string: expected {}, got {}",
            len,
            buf.remaining()
        )));
    }

    let bytes = buf
        .get(..len)
        .ok_or_else(|| JdwpError::Protocol("Not enough data for string".to_string()))?
        .to_vec();
    buf.advance(len);

    String::from_utf8(bytes)
        .map_err(|e| JdwpError::Protocol(format!("Invalid UTF-8 in string: {e}")))
}

/// Read a u32
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_u32(buf: &mut &[u8]) -> JdwpResult<u32> {
    if buf.remaining() < 4 {
        return Err(JdwpError::Protocol("Not enough data for u32".to_string()));
    }
    Ok(buf.get_u32())
}

/// Read a i32
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_i32(buf: &mut &[u8]) -> JdwpResult<i32> {
    if buf.remaining() < 4 {
        return Err(JdwpError::Protocol("Not enough data for i32".to_string()));
    }
    Ok(buf.get_i32())
}

/// Read a u8
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_u8(buf: &mut &[u8]) -> JdwpResult<u8> {
    if buf.remaining() < 1 {
        return Err(JdwpError::Protocol("Not enough data for u8".to_string()));
    }
    Ok(buf.get_u8())
}

/// Read a u64
///
/// # Errors
/// Returns a [`JdwpError`] if the buffer does not contain enough bytes or is malformed.
pub fn read_u64(buf: &mut &[u8]) -> JdwpResult<u64> {
    if buf.remaining() < 8 {
        return Err(JdwpError::Protocol("Not enough data for u64".to_string()));
    }
    Ok(buf.get_u64())
}
