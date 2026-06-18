#![no_main]

//! Coverage-guided fuzz of `aether_data::wire::from_bytes` over arbitrary
//! bytes, decoding into a discriminant-and-scalar type. The decoder's
//! contract is "any bytes → `Ok` or `Err`, never panic"; libFuzzer treats
//! any panic / abort as a crash, so the target calls the decoder and
//! discards the `Result`.
//!
//! `WireDiscriminants` leads with a multi-variant enum, so the
//! `variant_index` selector faults on byte zero (an out-of-range
//! discriminant must `Err`, not index past the variant table), then walks
//! `bool` (`InvalidBool`), `char` (`InvalidChar`), and a truncatable `u64`
//! (`UnexpectedEof`) — the validation paths a value-generating roundtrip
//! never reaches.

use aether_codec_fuzz::WireDiscriminants;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = aether_data::wire::from_bytes::<WireDiscriminants>(data);
});
