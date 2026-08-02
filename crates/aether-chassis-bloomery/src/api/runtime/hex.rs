//! The bloom-id URL codec: a digest is addressed in a path segment as 64
//! lowercase hex characters, so every `{id}` / `{digest}` route decodes through
//! here and every id rendered back into a response encodes through here.

use aether_bloomery::Digest;

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
mod tests {
    use super::{digest_from_hex, hex_encode};
    use aether_bloomery::Digest;

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
