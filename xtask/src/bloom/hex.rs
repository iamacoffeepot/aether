//! Hex digest spelling the coordinator REST edge speaks (64 lowercase characters).

use sha2::{Digest, Sha256};

/// A 32-byte digest spelled as 64 zero hex characters — the placeholder
/// approval `detail` a seal request has to be shaped with before the gate
/// overwrites it.
pub(super) const ZERO_DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Lowercase-hex-encode `bytes`.
pub(super) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("high nibble is 0..16"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("low nibble is 0..16"));
    }
    out
}

/// Whether `text` is exactly 32 bytes of hex.
pub(super) fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// sha256 of `bytes`, spelled as 64 lowercase hex characters.
pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    encode(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::{encode, is_digest, sha256_hex};

    #[test]
    fn encode_is_lowercase_hex() {
        assert_eq!(encode(&[0x00, 0x0f, 0xa5]), "000fa5");
    }

    #[test]
    fn is_digest_rejects_wrong_length_and_non_hex() {
        assert!(is_digest(&"ab".repeat(32)));
        assert!(!is_digest(&"ab".repeat(31)));
        assert!(!is_digest(&"g".repeat(64)));
    }

    #[test]
    fn sha256_hex_is_stable() {
        // Tripwire: intent / scope-revision defaults hash the task file; a
        // changed spelling would silently reseal every workpiece on a new
        // digest and drop the claim a successor is meant to carry.
        assert_eq!(sha256_hex(b"task"), "0ebb429fa86d481c2630fac53db1c91cffed5d4d41d1021c179444eb67e7ee0b");
    }
}
