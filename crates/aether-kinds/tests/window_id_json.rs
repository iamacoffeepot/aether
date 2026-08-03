//! Tripwire: a window id reaches JSON as an ADR-0064 tagged string
//! (iamacoffeepot/aether#4344).
//!
//! The round trip is not the assertion — a `u64` round-trips through
//! `serde_json` whatever its schema says, so a symmetric test would have
//! passed against the bug. What broke was the *shape*: `WindowId` declared
//! `Scalar(U64)`, so the codec rendered a lineage-fold id — around 2^60 —
//! as a bare JSON number, and any consumer parsing numbers as doubles
//! quantised it to the nearest multiple of 256. The id an agent read from
//! `aether.window.list` could not be handed back to `capture_frame`.
//!
//! So what is pinned is that the rendered value is a string, which is the
//! property that makes it safe for a consumer this process cannot see.

use aether_codec::{decode_schema, encode_schema};
use aether_data::{Kind, Schema};
use aether_kinds::{Key, WindowId};
use serde_json::{Value, json};

/// A real desktop window id: the ADR-0099 lineage fold of the window
/// actor, which is where in the `u64` range these actually land. Above
/// 2^53, and not a multiple of the 256-wide `f64` spacing up there — so a
/// value that has been through a double comes back different.
const LINEAGE_SCALE_ID: u64 = 1_473_705_000_037_674_430;

#[test]
fn a_window_id_renders_as_a_tagged_string() {
    let key = Key { window: WindowId(LINEAGE_SCALE_ID), code: 42 };
    let json = decode_schema(&key.encode_into_bytes(), &<Key as Schema>::SCHEMA).expect("a key decodes to JSON");

    let window = &json["window"];
    let rendered = window.as_str().unwrap_or_else(|| {
        panic!("a window id must render as a tagged string, not as {window} — a number above 2^53 does not survive a consumer that parses it as a double")
    });
    assert!(rendered.starts_with("mbx-"), "a window id is a mailbox id, so it carries that tag: got {rendered}");
}

#[test]
fn a_tagged_window_id_encodes_back_to_the_same_bits() {
    let key = Key { window: WindowId(LINEAGE_SCALE_ID), code: 42 };
    let schema = <Key as Schema>::SCHEMA;
    let rendered = decode_schema(&key.encode_into_bytes(), &schema).expect("a key decodes to JSON");

    let bytes = encode_schema(&rendered, &schema).expect("the rendered JSON encodes back");
    let round_tripped: Key = Key::decode_from_bytes(&bytes).expect("the re-encoded bytes decode as a key");

    assert_eq!(round_tripped.window, key.window, "the tagged form carries every bit of the id");
}

/// The codec's number arm stays open, so a caller that already sends a
/// plain integer — anything small enough for it to be exact — keeps
/// working. The tagged form is what a *reader* gets, not a new demand on
/// writers.
#[test]
fn a_plain_number_window_id_still_encodes() {
    let params: Value = json!({ "window": 7, "code": 42 });
    let bytes = encode_schema(&params, &<Key as Schema>::SCHEMA).expect("a numeric window id is still accepted");
    let key = Key::decode_from_bytes(&bytes).expect("the bytes decode as a key");

    assert_eq!(key.window, WindowId(7));
}
