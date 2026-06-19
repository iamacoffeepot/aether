#![no_main]

//! Coverage-guided fuzz of the `aether_data::wire` encode→decode→encode
//! byte fixed-point. The fuzzer generates arbitrary `WireValue` inputs;
//! for each one the target:
//!
//! 1. Encodes to bytes (`b1 = to_vec(&value)`).
//! 2. Decodes back (`v2 = from_bytes::<WireValue>(&b1).unwrap()`).
//! 3. Re-encodes (`b2 = to_vec(&v2).unwrap()`).
//! 4. Asserts `b1 == b2`.
//!
//! Byte comparison rather than value comparison is deliberate: the format is
//! bit-faithful for `f32`/`f64`, so an `Arbitrary` `NaN` survives the
//! roundtrip as identical bits while `value == v2` would be false. Comparing
//! the re-encoded bytes is `NaN`-safe and still catches any real
//! encode/decode asymmetry. This assertion also subsumes encode determinism:
//! if the same value produces different bytes on two calls, the fixed-point
//! would fail.
//!
//! `.unwrap()` on encode is safe because `to_vec` only errors when a
//! length exceeds the `u32` ceiling, which libFuzzer-sized inputs cannot
//! reach. `.unwrap()` on `from_bytes` is the actual roundtrip assertion:
//! freshly encoded bytes must decode without error.

use aether_codec_fuzz::WireValue;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|value: WireValue| {
    let b1 = aether_data::wire::to_vec(&value).unwrap();
    let v2: WireValue = aether_data::wire::from_bytes(&b1).unwrap();
    let b2 = aether_data::wire::to_vec(&v2).unwrap();
    assert_eq!(b1, b2);
});
