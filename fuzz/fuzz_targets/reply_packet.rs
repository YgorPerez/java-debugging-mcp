//! The whole-packet entry point: `ReplyPacket::decode` (TEST-45, #153).
//!
//! What this is actually about is availability and correctness rather than memory safety — there is no
//! `unsafe` in this crate. A panic here drops a session, and this decoder reads a length and a flag off
//! the wire before doing anything else, so it is the first place a lost byte becomes a wrong answer.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The contract is "never panic". A `Err` is a correct outcome and so is an `Ok` on anything that
    // happens to be well formed; only unwinding is a finding.
    let _ = jdwp_client::protocol::ReplyPacket::decode(data);
});
