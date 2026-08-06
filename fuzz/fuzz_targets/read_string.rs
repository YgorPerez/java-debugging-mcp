//! A length prefix that lies (TEST-45, #153).
//!
//! `read_string` takes a u32 off the wire and then reads that many bytes. `reader.rs` already defends it
//! with `ensure` plus a checked slice, and a unit test covers empty/truncated/lying lengths by hand — this
//! is the same claim without the hand-picked cases.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut buf = data;
    let _ = jdwp_client::reader::read_string(&mut buf);
});
