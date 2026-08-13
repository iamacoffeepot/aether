//! The REST edge's digest form: 64 lowercase hex characters, in a path segment
//! and in a JSON body alike.
//!
//! A digest addressed in a path segment has always been hex — every `{id}` /
//! `{digest}` route decodes through [`digest_from_hex`] and every id rendered
//! back into a response encodes through [`hex_encode`]. The body codecs next
//! door ([`from_slice`], [`to_vec`]) give a body the same spelling, so an
//! operator authors and reads one representation of a digest across the whole
//! surface instead of typing `"base": [185, 103, …]` into the body of a request
//! whose path segment names the same kind of value in hex.
//!
//! The hex form lives only at this edge. Both codecs resolve it into (and
//! render it from) the canonical 32 bytes before anything downstream sees the
//! value, so the wire encoding, the digests computed over it, and the journal
//! are untouched — the same split `aether-mcp` keeps between its `$`-sigil blob
//! embeds and the strict wire codec behind them.

mod deserialize;
mod serialize;

pub(super) use deserialize::from_slice;
pub(super) use serialize::to_vec;

use aether_bloomery::Digest;

/// The name [`Digest`] reports for itself as a `serde` newtype struct — the
/// hook both body codecs key on to recognize a digest by type rather than by
/// the shape of its rendered JSON.
const DIGEST: &str = "Digest";

/// Lowercase-hex-encode bytes (bloom ids in URLs).
pub(super) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("high nibble is 0..16"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("low nibble is 0..16"));
    }
    out
}

/// Decode a lowercase/uppercase hex string of exactly 32 bytes into a digest.
pub(super) fn digest_from_hex(hex: &str) -> Option<Digest> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let high = hex.as_bytes()[index * 2];
        let low = hex.as_bytes()[index * 2 + 1];
        *slot = (hex_nibble(high)? << 4) | hex_nibble(low)?;
    }
    Some(Digest::from_bytes(bytes))
}

/// One hex digit to its nibble value.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{Digest, Workpiece, WorkpieceId};
    use aether_data::wire;
    use serde_json::json;

    use super::{digest_from_hex, from_slice, hex_encode, to_vec};
    use crate::api::dto::DraftPatch;

    /// The base digest a draft is patched with, in both spellings.
    const BASE: Digest = Digest::from_bytes([0x5c; 32]);

    #[test]
    fn a_draft_patch_takes_its_base_in_either_spelling() {
        let canonical = json!({ "base": BASE.as_bytes() }).to_string();
        let hex = json!({ "base": hex_encode(BASE.as_bytes()) }).to_string();

        let from_canonical: DraftPatch = from_slice(canonical.as_bytes()).unwrap();
        let from_hex: DraftPatch = from_slice(hex.as_bytes()).unwrap();

        assert_eq!(from_canonical.base, Some(BASE), "the canonical byte array is still the wire form");
        assert_eq!(from_hex.base, Some(BASE), "and 64 hex characters name the same digest");
    }

    #[test]
    fn either_spelling_decodes_to_the_same_canonical_wire_bytes() {
        // Tripwire: hex is a spelling this edge accepts, never a second
        // encoding. Both forms have to arrive at bytes whose canonical wire
        // encoding hashes to exactly this — so a hex path that produced
        // anything but the 32 bytes it names, or a change to the encoding
        // underneath the REST edge, fails here instead of silently moving every
        // digest computed over the value.
        let staged = |revision: serde_json::Value| -> Workpiece {
            from_slice(
                json!({ "id": "wp-a", "intent": BASE.as_bytes(), "scope_revision": revision }).to_string().as_bytes(),
            )
            .unwrap()
        };

        let from_canonical = staged(json!(BASE.as_bytes()));
        let from_hex = staged(json!(hex_encode(BASE.as_bytes())));

        assert_eq!(
            from_canonical,
            Workpiece { id: WorkpieceId("wp-a".to_owned()), intent: BASE, scope_revision: BASE }
        );
        assert_eq!(from_hex, from_canonical, "the two spellings are one value");
        assert_eq!(
            hex_encode(Digest::of_wire_bytes(&wire::to_vec(&from_hex).unwrap()).as_bytes()),
            "d6d830ebdc12bcd7032b51f0a3dc88d1753984e463a76fa6ee388639d26be35e"
        );
    }

    #[test]
    fn a_rendered_digest_reads_as_hex() {
        let rendered =
            String::from_utf8(to_vec(&DraftPatch { base: Some(BASE), ..DraftPatch::default() }).unwrap()).unwrap();

        assert_eq!(rendered, format!(r#"{{"base":"{}"}}"#, hex_encode(BASE.as_bytes())));
    }

    #[test]
    fn hex_round_trips_a_digest() {
        // The bloom-id URL encoding: 32 bytes → 64 lowercase hex chars → back to
        // the same 32 bytes. Catches a nibble-order or length bug in the hand-
        // rolled hex the id routes depend on.
        let digest = Digest::from_bytes([
            0x00, 0x0f, 0x10, 0xff, 0xa5, 0x5a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x1f, 0x2e, 0x3d, 0x4c, 0x5b, 0x6a, 0x79, 0x88, 0x97, 0xa6, 0xb5, 0xc4,
        ]);
        let hex = hex_encode(digest.as_bytes());
        assert_eq!(hex.len(), 64);
        assert_eq!(digest_from_hex(&hex), Some(digest));
    }

    #[test]
    fn digest_from_hex_rejects_bad_input() {
        // A 63/65-char string and a non-hex char are both rejected rather than
        // silently truncated or mis-decoded into a wrong bloom id.
        assert_eq!(digest_from_hex(&"a".repeat(63)), None);
        assert_eq!(digest_from_hex(&"a".repeat(65)), None);
        assert_eq!(digest_from_hex(&"g".repeat(64)), None);
    }
}
