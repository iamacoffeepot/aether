#![no_main]

//! Coverage-guided fuzz of `aether_data::wire::from_bytes` over arbitrary
//! bytes, decoding into a string-only type. The decoder's contract is "any
//! bytes → `Ok` or `Err`, never panic"; libFuzzer treats any panic / abort
//! as a crash, so the target calls the decoder and discards the `Result`.
//!
//! `WireStrings` is the string length-prefix + UTF-8 surface with nothing
//! ahead of it, so the fuzzer hits the `Length` and `Utf8` error paths on
//! byte zero instead of after the scalar/option fields a composite type
//! would interpose.

use aether_codec_fuzz::WireStrings;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = aether_data::wire::from_bytes::<WireStrings>(data);
});
