//! [`NamespacePath`]: a path relative to a file namespace's root, valid by
//! construction on every way in, wire decode and `Deserialize` included.

use std::error::Error as StdError;
use std::fmt;

use aether_data::schema::{LabelNode, SchemaType};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{CrossesActors, CrossesWire, Schema};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A path relative to a file namespace's root.
///
/// The grammar: one or more segments joined by single `/`, with no leading
/// or trailing `/`, no empty segment, and no segment equal to `.` or `..`.
/// Every byte of a segment is one of `a-z`, `0-9`, `.`, `_`, `-`. That one
/// byte rule is the whole character policy: it is ASCII-only, so uppercase,
/// backslash, NUL, `:`, whitespace, control bytes and every non-ASCII byte
/// are refused by the same test. Lowercase only, because a case-insensitive
/// filesystem would otherwise give one file several names. A segment is at
/// most [`MAX_SEGMENT_BYTES`](Self::MAX_SEGMENT_BYTES) long and a path at
/// most [`MAX_BYTES`](Self::MAX_BYTES).
///
/// Ordering is the byte order of the text.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NamespacePath(Box<str>);

impl NamespacePath {
    /// The longest path, in bytes.
    pub const MAX_BYTES: usize = 1024;
    /// The longest segment, in bytes.
    pub const MAX_SEGMENT_BYTES: usize = 255;

    /// Validate `text` against the grammar.
    ///
    /// # Errors
    ///
    /// Returns the [`NamespacePathError`] naming the first rule `text`
    /// breaks.
    pub fn new(text: &str) -> Result<Self, NamespacePathError> {
        if text.is_empty() {
            return Err(NamespacePathError::Empty);
        }
        if text.len() > Self::MAX_BYTES {
            return Err(NamespacePathError::TooLong);
        }
        for segment in text.split('/') {
            check_segment(segment.as_bytes())?;
        }
        Ok(Self(text.into()))
    }

    /// The validated text.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        &self.0
    }
}

/// Check one segment of a path: non-empty, not `.` or `..`, within the
/// length bound, and made only of the permitted bytes.
fn check_segment(segment: &[u8]) -> Result<(), NamespacePathError> {
    if segment.is_empty() {
        return Err(NamespacePathError::EmptySegment);
    }
    if matches!(segment, b"." | b"..") {
        return Err(NamespacePathError::DotSegment);
    }
    if segment.len() > NamespacePath::MAX_SEGMENT_BYTES {
        return Err(NamespacePathError::SegmentTooLong);
    }
    for byte in segment {
        if !is_segment_byte(*byte) {
            return Err(NamespacePathError::Byte(*byte));
        }
    }
    Ok(())
}

/// Whether `byte` may appear in a segment.
const fn is_segment_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
}

impl fmt::Display for NamespacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// [`NamespacePath::new`] rejection: which rule the text broke.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NamespacePathError {
    /// The text is empty.
    Empty,
    /// The text is longer than [`NamespacePath::MAX_BYTES`].
    TooLong,
    /// A segment is empty: a leading, trailing, or doubled `/`.
    EmptySegment,
    /// A segment is `.` or `..`.
    DotSegment,
    /// A segment is longer than [`NamespacePath::MAX_SEGMENT_BYTES`].
    SegmentTooLong,
    /// The first byte outside the segment set `a-z`, `0-9`, `.`, `_`, `-`.
    Byte(u8),
}

impl fmt::Display for NamespacePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("invalid namespace path: it is empty"),
            Self::TooLong => {
                write!(f, "invalid namespace path: it is longer than {} bytes", NamespacePath::MAX_BYTES)
            }
            Self::EmptySegment => {
                f.write_str("invalid namespace path: a segment is empty (a leading, trailing, or doubled `/`)")
            }
            Self::DotSegment => f.write_str("invalid namespace path: a segment is `.` or `..`"),
            Self::SegmentTooLong => {
                write!(f, "invalid namespace path: a segment is longer than {} bytes", NamespacePath::MAX_SEGMENT_BYTES)
            }
            Self::Byte(byte) => {
                write!(f, "invalid namespace path: byte {byte:#04x} is not one of `a-z`, `0-9`, `.`, `_`, `-`")
            }
        }
    }
}

impl StdError for NamespacePathError {}

impl Schema for NamespacePath {
    const SCHEMA: SchemaType = SchemaType::String;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl CrossesActors for NamespacePath {}
impl CrossesWire for NamespacePath {}

impl WireEncode for NamespacePath {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.as_str().encode(out)
    }
}

impl<'de> WireDecode<'de> for NamespacePath {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::new(&String::decode(cursor)?).map_err(|error| WireError::Message(format!("aether wire: {error}")))
    }
}

impl Serialize for NamespacePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NamespacePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(&<Box<str>>::deserialize(deserializer)?).map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{encode_to_vec, from_bytes};

    use super::*;

    /// Each refused spelling breaks exactly one rule, so dropping a rule
    /// from the check admits its row.
    #[test]
    fn namespace_path_refuses_each_malformed_spelling() {
        let long_segment = "a".repeat(NamespacePath::MAX_SEGMENT_BYTES + 1);
        let long_path = ["abcdefghi"; 103].join("/");
        let refused = [
            ("", NamespacePathError::Empty),
            ("/a", NamespacePathError::EmptySegment),
            ("a/", NamespacePathError::EmptySegment),
            ("a//b", NamespacePathError::EmptySegment),
            (".", NamespacePathError::DotSegment),
            ("a/../b", NamespacePathError::DotSegment),
            ("a\\b", NamespacePathError::Byte(b'\\')),
            ("a:b", NamespacePathError::Byte(b':')),
            ("a\0b", NamespacePathError::Byte(0)),
            ("a/Bc", NamespacePathError::Byte(b'B')),
            ("caf\u{e9}", NamespacePathError::Byte(0xc3)),
            ("a b", NamespacePathError::Byte(b' ')),
            (long_segment.as_str(), NamespacePathError::SegmentTooLong),
            (long_path.as_str(), NamespacePathError::TooLong),
        ];

        for (text, error) in refused {
            assert_eq!(NamespacePath::new(text), Err(error), "{text:?}");
        }
        let accepted = NamespacePath::new("modules/50_50/square-0.v2.wasm").expect("a nested path is accepted");
        assert_eq!(accepted.as_str(), "modules/50_50/square-0.v2.wasm");
    }

    /// Both decodes run the constructor: a spelling it refuses must not
    /// come out of the wire as a value.
    #[test]
    fn namespace_path_decodes_refuse_what_the_constructor_refuses() {
        for refused in ["", "a/../b", "A", "a//b"] {
            let bytes = encode_to_vec(refused).expect("a string encodes");
            let mut cursor: &[u8] = &bytes;

            assert!(NamespacePath::decode(&mut cursor).is_err(), "WireDecode refuses {refused:?}");
            assert!(from_bytes::<NamespacePath>(&bytes).is_err(), "Deserialize refuses {refused:?}");
        }
    }
}
