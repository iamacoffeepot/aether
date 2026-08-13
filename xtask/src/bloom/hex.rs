//! 32-byte digest spelling the REST edge already speaks: 64 hex characters.

use anyhow::{Result, bail};

/// Lowercase-hex-encode bytes.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("high nibble is 0..16"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("low nibble is 0..16"));
    }
    out
}

/// Decode a 64-character hex string into 32 bytes.
pub fn decode(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
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
        assert_eq!(decode(&hex.to_ascii_uppercase()), Some(bytes));
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert_eq!(decode(&"a".repeat(63)), None);
        assert_eq!(decode(&"a".repeat(65)), None);
        assert_eq!(decode(&"g".repeat(64)), None);
    }
}
