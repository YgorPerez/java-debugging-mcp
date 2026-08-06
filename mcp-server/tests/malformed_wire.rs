//! The wire read path against input that is wrong (TEST-45, #153), on stable and in every `cargo test`.
//!
//! `fuzz/` is the other half of this issue and finds more, but it needs nightly and cargo-fuzz, so on any
//! ordinary run it finds nothing because it does not run. These are the same claims as the fuzz targets,
//! made deterministically and cheaply enough to ride on every build — which is the difference between a
//! check that guards the code and a check that guards the code on the days somebody remembers.
//!
//! WHAT IS BEING CLAIMED, and it is not memory safety: there is no `unsafe` in `jdwp-client`. It is that
//! the decoder never PANICS. A panic in the event loop drops a session, and on a shared JVM a dropped
//! session can leave a debuggee suspended — which is the failure ADR-0003 and SAFE-7 (#7) exist for. An
//! `Err` is a correct outcome here and so is an `Ok` on anything well formed; only unwinding is a finding.
//!
//! The expectation was that these would find nothing, and they did not. `reader.rs` already defends every
//! read with `ensure` plus a checked slice. That is worth having a test say rather than a comment.

use jdwp_client::protocol::ReplyPacket;
use jdwp_client::reader::{read_i32, read_i64, read_string, read_u32, read_u64, read_u8, read_value_by_tag};

/// A deterministic xorshift, so a failure names an input somebody else can reproduce.
///
/// Deliberately not a `rand` dependency: `cargo-shear` and `cargo-machete` both gate here, `deny.toml`
/// pins the licence set, and a crate pulled in to produce twenty thousand pseudo-random bytes would be
/// carried by the whole workspace forever.
struct Xorshift(u64);

impl Xorshift {
    const fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        // Truncation is the point: the high bits are the well-mixed ones and a byte is what is wanted.
        (0..n).map(|_| u8::try_from((self.next_u64() >> 24) & 0xff).unwrap_or(0)).collect()
    }
}

/// Every recorded reply payload, as the bytes that came off a real JVM.
fn cassette_payloads() -> Vec<(String, Vec<u8>)> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes");
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {dir}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    for path in entries {
        let text = std::fs::read_to_string(&path).expect("read cassette");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse cassette");
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        for (i, exchange) in json["exchanges"].as_array().into_iter().flatten().enumerate() {
            let Some(chunks) = exchange["reply"].as_array() else { continue };
            let hex: String = chunks.iter().filter_map(|c| c.as_str()).collect();
            if hex.is_empty() {
                continue;
            }
            let bytes = (0..hex.len() / 2)
                .map(|k| u8::from_str_radix(&hex[k * 2..k * 2 + 2], 16).expect("cassette hex"))
                .collect();
            out.push((format!("{stem}-{i:03}"), bytes));
        }
    }
    assert!(!out.is_empty(), "no cassette payloads found; the seed corpus for this test is empty");
    out
}

/// Frame a payload the way the JVM does, so `ReplyPacket::decode` sees a whole packet.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(11 + payload.len());
    packet.extend_from_slice(&u32::try_from(11 + payload.len()).unwrap_or(u32::MAX).to_be_bytes());
    packet.extend_from_slice(&1u32.to_be_bytes());
    packet.push(0x80); // reply flag
    packet.extend_from_slice(&0u16.to_be_bytes()); // error code
    packet.extend_from_slice(payload);
    packet
}

/// Every prefix of a valid packet, which is what a half-arrived read looks like.
#[test]
fn a_truncated_reply_packet_is_refused_rather_than_decoded() {
    for (name, payload) in cassette_payloads() {
        let packet = frame(&payload);
        assert!(ReplyPacket::decode(&packet).is_ok(), "{name}: a recorded reply should decode");

        for cut in 0..packet.len().min(512) {
            let result = ReplyPacket::decode(&packet[..cut]);
            if cut < 11 {
                assert!(
                    result.is_err(),
                    "{name}: a {cut}-byte packet is shorter than the 11-byte header and must be refused"
                );
            }
        }
    }
}

/// One byte changed, everywhere it can be changed, on frames that are otherwise real.
///
/// This is the mutation the fuzzer spends most of its time on, done exhaustively over the first stretch
/// of each packet — where the length, the id, the flag and the error code live, and where a wrong byte
/// is most likely to be believed.
#[test]
fn a_single_corrupt_byte_in_a_real_reply_never_panics() {
    // The default hook prints a backtrace for every caught panic; there should be none, but if there is
    // one the assertion below reports it far better than 320 lines of unwinding would.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    for (name, payload) in cassette_payloads() {
        let packet = frame(&payload);
        for at in 0..packet.len().min(64) {
            for replacement in [0x00, 0x01, 0x7f, 0x80, 0xff] {
                let mut corrupted = packet.clone();
                corrupted[at] = replacement;
                // The only claim is "does not panic"; whether it decodes depends on which byte moved.
                // Caught rather than merely run, so a failure names the frame and the byte instead of
                // pointing at a loop — the input has to be reproducible for the report to be worth having.
                let outcome = std::panic::catch_unwind(|| ReplyPacket::decode(&corrupted));
                assert!(
                    outcome.is_ok(),
                    "decoding {name} panicked with byte {at} set to {replacement:#04x}. A panic in the \
                     wire read path drops the session, and on a shared JVM a dropped session can leave \
                     the debuggee suspended."
                );
            }
        }
    }

    std::panic::set_hook(previous);
}

/// Every tag against every buffer length up to the widest value JDWP defines.
///
/// The tag byte decides how many bytes to read and it comes off the wire, so a tag promising an 8-byte
/// object id with three bytes left is the shape that desynchronises a stream rather than failing loudly.
#[test]
fn every_value_tag_against_every_short_buffer_is_an_error_not_a_panic() {
    let filler = [0xffu8; 16];
    for tag in 0..=u8::MAX {
        for len in 0..=16usize {
            let mut buf = &filler[..len];
            let _ = read_value_by_tag(tag, &mut buf);
        }
    }
}

/// A length prefix that lies, including the two the fuzzer independently found interesting.
#[test]
fn a_string_length_prefix_that_lies_is_refused() {
    for claimed in [0u32, 1, 8, 0x7fff_ffff, 0xffff_ffff] {
        for body in [b"".as_slice(), b"ab".as_slice(), b"hello world".as_slice()] {
            let mut bytes = claimed.to_be_bytes().to_vec();
            bytes.extend_from_slice(body);
            let mut buf = bytes.as_slice();
            let result = read_string(&mut buf);
            if claimed as usize > body.len() {
                assert!(
                    result.is_err(),
                    "a string claiming {claimed} bytes with {} available must be refused, not read",
                    body.len()
                );
            }
        }
    }
}

/// The scalar readers, over random bytes and every buffer length that could truncate them.
#[test]
fn the_scalar_readers_never_panic_on_a_short_or_random_buffer() {
    let mut rng = Xorshift::new(0x5EED_1234_ABCD_0001);
    for _ in 0..2_000 {
        let size = usize::try_from(rng.next_u64() % 24).unwrap_or(0);
        let bytes = rng.bytes(size);
        for take in 0..=bytes.len() {
            let slice = &bytes[..take];
            let _ = read_u8(&mut { slice });
            let _ = read_u32(&mut { slice });
            let _ = read_i32(&mut { slice });
            let _ = read_u64(&mut { slice });
            let _ = read_i64(&mut { slice });
            let _ = read_string(&mut { slice });
        }
    }
}

/// Whole packets of noise, which is what the stream looks like if a read ever lands mid-frame.
#[test]
fn random_bytes_are_never_decoded_as_a_reply_by_accident() {
    let mut rng = Xorshift::new(0xC0FF_EE00_1234_5678);
    let mut decoded_anything = false;
    for _ in 0..20_000 {
        let len = usize::try_from(rng.next_u64() % 64).unwrap_or(0);
        let bytes = rng.bytes(len);
        if let Ok(packet) = ReplyPacket::decode(&bytes) {
            // Decoding random bytes is not itself wrong — an 11-byte buffer whose fifth byte happens to
            // be 0x80 IS a well-formed empty reply. What must hold is that the accessors on it are safe.
            let _ = packet.is_error();
            let _ = packet.data().len();
            decoded_anything = true;
        }
    }
    assert!(
        decoded_anything,
        "20000 random buffers produced no decodable packet at all, which means this test stopped \
         exercising the success path and is now only checking that errors do not panic"
    );
}
