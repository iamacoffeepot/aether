//! [`Sha256`] and [`NamedObject`]: the identity of a package object and the
//! row a package's table of named objects holds for one (ADR-0163 §1).

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

/// A sha256 content address — the identity of a package object (ADR-0163
/// §1). Encoded on the wire as its 32 raw bytes; rendered as lowercase hex
/// for the `pack/objects/<hash>` filename. The engine never hashes bytes
/// against it (integrity is the platform's job) — it parses hashes out of
/// the manifest and reads the named file — so this newtype carries
/// render/parse but no digest computation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256(pub [u8; 32]);

impl Sha256 {
    /// Render as lowercase hex — the `pack/objects/<hash>` filename.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(self.0.len() * 2);
        for byte in self.0 {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Parse a 64-character lowercase-or-uppercase hex string into a hash.
    ///
    /// # Errors
    ///
    /// [`Sha256ParseError::BadLength`] if the string is not 64 hex digits;
    /// [`Sha256ParseError::BadDigit`] if a character is not a hex digit.
    pub fn from_hex(s: &str) -> Result<Self, Sha256ParseError> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(Sha256ParseError::BadLength(bytes.len()));
        }
        let mut out = [0u8; 32];
        for (index, pair) in bytes.chunks_exact(2).enumerate() {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            out[index] = (high << 4) | low;
        }
        Ok(Self(out))
    }
}

fn hex_digit(byte: u8) -> Result<u8, Sha256ParseError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(Sha256ParseError::BadDigit(other)),
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({})", self.to_hex())
    }
}

/// A failure parsing a hex string into a [`Sha256`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sha256ParseError {
    /// The string was not exactly 64 hex digits (the found length).
    BadLength(usize),
    /// A character was not a hex digit (the offending byte).
    BadDigit(u8),
}

impl fmt::Display for Sha256ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadLength(len) => write!(f, "sha256 hex must be 64 digits, got {len}"),
            Self::BadDigit(byte) => write!(f, "sha256 hex holds a non-hex byte {byte:#04x}"),
        }
    }
}

impl Error for Sha256ParseError {}

/// One object a package ships that boot checks for and does not load,
/// reached by a running engine at the path it is keyed under in the
/// package's table (ADR-0163 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedObject {
    /// The object's identity, and its file name under `pack/objects`.
    pub sha256: Sha256,
    /// The object's length in bytes.
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_round_trips() {
        let hash = Sha256([0x0f; 32]);
        assert_eq!(hash.to_hex().len(), 64);
        assert_eq!(Sha256::from_hex(&hash.to_hex()).expect("parse"), hash);
    }

    #[test]
    fn sha256_from_hex_rejects_bad_input() {
        assert_eq!(Sha256::from_hex("abc"), Err(Sha256ParseError::BadLength(3)));
        let mut sixty_four = "0".repeat(63);
        sixty_four.push('z');
        assert_eq!(Sha256::from_hex(&sixty_four), Err(Sha256ParseError::BadDigit(b'z')));
    }
}
