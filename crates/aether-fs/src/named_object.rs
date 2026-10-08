//! [`Sha256`] and [`NamedObject`]: the identity of a package object and the
//! row a package's table of named objects holds for one (ADR-0163 §1), and
//! [`nested_object_paths`], the check that no object's path is also a
//! directory of another.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

use crate::NamespacePath;

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

/// Two paths of one table where the first names an object and is also an
/// ancestor of the second, which a real directory cannot mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedObjectPaths<'table> {
    /// The path that names an object and is also an ancestor of another.
    pub object: &'table NamespacePath,
    /// The path of an object under it.
    pub beneath: &'table NamespacePath,
}

/// The first pair in `named` where one path is a proper ancestor, at a `/`
/// boundary, of another, or `None` when no directory-shaped conflict exists.
///
/// A directory cannot hold a file and a directory under one name, so a table
/// with such a pair cannot be mirrored by one.
#[must_use]
pub fn nested_object_paths<V>(named: &BTreeMap<NamespacePath, V>) -> Option<NestedObjectPaths<'_>> {
    named.keys().find_map(|beneath| {
        ancestors(beneath.as_str())
            .find_map(|ancestor| named.get_key_value(ancestor))
            .map(|(object, _)| NestedObjectPaths { object, beneath })
    })
}

/// The text before each `/` of `path`, shortest first.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each case is a bug in a plausible shortcut: comparing only neighbouring
    /// keys misses `a`, `a/b` because `a-b` and `a.b` sort between them;
    /// a byte-prefix test with no `/` boundary refuses `a` beside `ab/c`; and
    /// checking only the parent misses `a/b` above `a/b/c/d`.
    #[test]
    fn nested_object_paths_finds_an_ancestor_at_a_slash_boundary() {
        let table = |paths: &[&str]| -> BTreeMap<NamespacePath, u8> {
            paths.iter().map(|text| (NamespacePath::new(text).expect("a valid path"), 0)).collect()
        };
        let found = |paths: &[&str]| {
            nested_object_paths(&table(paths)).map(|nested| (nested.object.to_string(), nested.beneath.to_string()))
        };

        assert_eq!(found(&["a", "a-b", "a.b", "a/b"]), Some(("a".to_string(), "a/b".to_string())));
        assert_eq!(found(&["a", "a-b", "a.b", "ab/c"]), None);
        assert_eq!(found(&["a/b", "a/b/c/d"]), Some(("a/b".to_string(), "a/b/c/d".to_string())));
    }

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
