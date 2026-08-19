//! 32-byte digest spelling the REST edge already speaks: 64 hex characters.

use aether_bloomery::Digest;
use anyhow::{Result, bail};

/// Lowercase-hex-encode bytes.
pub fn encode(bytes: &[u8]) -> String {
    aether_bloomery::encode_hex(bytes)
}

/// Decode a 64-character lowercase hex string into 32 bytes.
pub fn decode(hex: &str) -> Option<[u8; 32]> {
    Digest::from_hex(hex).map(|digest| *digest.as_bytes())
}

/// Decode or refuse with a named field.
pub fn decode_named(hex: &str, what: &str) -> Result<[u8; 32]> {
    decode(hex).ok_or_else(|| anyhow::anyhow!("{what} is not a 32-byte hex digest"))
}

/// Accept the REST edge's two spellings: 64 hex characters, or a 32-byte array.
pub fn from_json(value: &serde_json::Value, what: &str) -> Result<[u8; 32]> {
    match value {
        serde_json::Value::String(hex) => decode_named(hex, what),
        serde_json::Value::Array(items) if items.len() == 32 => {
            let mut bytes = [0u8; 32];
            for (slot, item) in bytes.iter_mut().zip(items) {
                let number = item.as_u64().ok_or_else(|| anyhow::anyhow!("{what} is not a 32-byte digest"))?;
                *slot = u8::try_from(number).map_err(|_| anyhow::anyhow!("{what} is not a 32-byte digest"))?;
            }
            Ok(bytes)
        }
        _ => bail!("{what} is not a 32-byte hex digest"),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn hex_round_trips_32_bytes() {
        let bytes = [0x5c; 32];
        let hex = encode(&bytes);
        assert_eq!(hex.len(), 64);
        assert_eq!(decode(&hex), Some(bytes));
        assert_eq!(decode(&hex.to_ascii_uppercase()), None);
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert_eq!(decode(&"a".repeat(63)), None);
        assert_eq!(decode(&"a".repeat(65)), None);
        assert_eq!(decode(&"g".repeat(64)), None);
    }

    #[test]
    fn decode_refuses_a_signed_prefix() {
        // Tripwire: `from_str_radix("+a", 16)` is 10. A digest id must not
        // admit that spelling onto `--base`.
        let mut hex = String::from("+a");
        hex.push_str(&"0".repeat(62));
        assert_eq!(decode(&hex), None);
    }
}
