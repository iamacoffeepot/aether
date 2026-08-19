//! Hex spelling for digests the operator types on the command line.

#[cfg(test)]
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex of `bytes`.
#[cfg(test)]
pub(super) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Decode a 64-character hex digest, or `None` when the spelling is not 32 bytes.
pub(super) fn decode_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = (nibble(hex.as_bytes()[index * 2])? << 4) | nibble(hex.as_bytes()[index * 2 + 1])?;
    }
    Some(bytes)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_digest, encode};

    #[test]
    fn round_trips_thirty_two_bytes() {
        let bytes = [0x5c; 32];
        let hex = encode(&bytes);
        let Some(decoded) = decode_digest(&hex) else {
            panic!("a hex spelling this encoder just wrote must decode");
        };
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn a_short_or_non_hex_spelling_is_not_a_digest() {
        assert!(decode_digest("abcd").is_none(), "too short");
        assert!(decode_digest(&"g".repeat(64)).is_none(), "not hex");
    }
}
