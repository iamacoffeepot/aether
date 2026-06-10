#![no_main]

//! Coverage-guided fuzz of `aether_codec::decode_schema`, the
//! substrate's untrusted-input boundary. The input's leading byte
//! selects a fixed schema from the table; the remaining bytes are the
//! adversarial wire payload. The decoder's contract is "any bytes,
//! valid schema": it must return `Ok` or `Err`, never panic. libFuzzer
//! treats any panic / abort as a crash, so the target body just calls
//! the decoder and ignores the `Result`.

use aether_codec_fuzz::schema_for;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };
    let schema = schema_for(selector);
    let _ = aether_codec::decode_schema(payload, &schema);
});
