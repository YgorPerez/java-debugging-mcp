//! Every JDWP value tag against a buffer that may not hold what the tag promises (TEST-45, #153).
//!
//! The tag byte decides how many bytes to read, and it comes off the wire. A tag claiming an object id
//! with four bytes left is the shape that turns one short reply into a desynchronised stream.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&tag, rest)) = data.split_first() else { return };
    let mut buf = rest;
    let _ = jdwp_client::reader::read_value_by_tag(tag, &mut buf);
});
