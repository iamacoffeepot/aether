#![no_main]

//! Coverage-guided fuzz of `aether_data::wire::from_bytes` over arbitrary
//! bytes, decoding into a collection-only type. The decoder's contract is
//! "any bytes → `Ok` or `Err`, never panic"; libFuzzer treats any panic /
//! abort as a crash, so the target calls the decoder and discards the
//! `Result`.
//!
//! `WireSeqs` is the collection length-prefix surface (`Vec`, byte
//! sequence, map) with the `Vec<u32>` length read first, so a length that
//! claims more elements than the buffer holds must fault as `Length` on
//! byte zero rather than over-read or pre-allocate unboundedly.

use aether_codec_fuzz::WireSeqs;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = aether_data::wire::from_bytes::<WireSeqs>(data);
});
