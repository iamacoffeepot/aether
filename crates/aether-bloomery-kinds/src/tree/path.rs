//! Inline symlink target. Valid by construction.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, LabelNode, Schema, SchemaType, StorageLeaves};

use crate::tree::name::{Name, NameError};

const PATH_MAX_BYTES: usize = 1024;

/// Why [`Path::new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The string was empty.
    Empty,
    /// The string was longer than 1024 bytes.
    TooLong,
    /// The string began with `/`.
    Absolute,
    /// The string contained an empty segment (`//` or a trailing `/`).
    EmptySegment,
    /// A segment failed a [`Name`] rule. `.` and `..` are allowed.
    Segment(NameError),
}

impl PathError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::Absolute => "absolute",
            Self::EmptySegment => "empty-segment",
            Self::Segment(error) => error.reason(),
        }
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Segment(error) => write!(f, "segment: {error}"),
            other => f.write_str(other.reason()),
        }
    }
}

impl StdError for PathError {}

/// A symlink target stored inline in the tree, not as a separate blob.
/// Relative, `/`-separated, and each segment is `.`, `..`, or a [`Name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(String);

impl Path {
    /// Accept a relative target whose segments are `.`, `..`, or a valid [`Name`].
    ///
    /// # Errors
    ///
    /// [`PathError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, PathError> {
        let value = value.into();
        if value.is_empty() {
            return Err(PathError::Empty);
        }
        if value.len() > PATH_MAX_BYTES {
            return Err(PathError::TooLong);
        }
        if value.starts_with('/') {
            return Err(PathError::Absolute);
        }
        if value.ends_with('/') || value.contains("//") {
            return Err(PathError::EmptySegment);
        }
        for segment in value.split('/') {
            if segment == "." || segment == ".." {
                continue;
            }
            if let Err(error) = Name::new(segment) {
                return Err(PathError::Segment(error));
            }
        }
        Ok(Self(value))
    }

    /// Borrow the target as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn invariant(error: PathError) -> StorageError {
    StorageError::Invariant { kind: "Path", reason: error.reason() }
}

fn wire_error(error: PathError) -> WireError {
    WireError::Message(error.reason().into())
}

impl Schema for Path {
    const SCHEMA: SchemaType = <String as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Path"));
    const LABEL_NODE: LabelNode = <String as Schema>::LABEL_NODE;
}

impl StorageLeaves for Path {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.0.contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Self::new(<String as StorageLeaves>::assemble(carry, depth, source)?).map_err(invariant)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        <String as StorageLeaves>::is_absent(carry, depth, source)
    }
}

impl WireEncode for Path {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for Path {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::new(String::decode(cursor)?).map_err(wire_error)
    }
}

impl StorageElement for Path {
    const TAGGED: bool = <String as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.0.contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Self::new(String::assemble_element(depth, cursor)?).map_err(invariant)
    }
}

impl Cites for Path {
    fn cites(&self, _sink: &mut Citations) {}
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{PATH_MAX_BYTES, Path, PathError};
    use crate::tree::name::NameError;

    fn path_of_len(len: usize) -> String {
        let mut path = "aa/".repeat(len / 3);
        path.push_str(&"a".repeat(len % 3));
        debug_assert_eq!(path.len(), len);
        path
    }

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        let too_long = path_of_len(PATH_MAX_BYTES + 1);
        let max_len = path_of_len(PATH_MAX_BYTES);
        let cases = [
            ("", PathError::Empty, "a"),
            (too_long.as_str(), PathError::TooLong, max_len.as_str()),
            ("/bin/run", PathError::Absolute, "../bin/run"),
            ("foo/", PathError::EmptySegment, "foo"),
            ("nul.txt", PathError::Segment(NameError::Device), "null.txt"),
        ];
        for (reject, error, accept) in cases {
            assert_eq!(Path::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(Path::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }
    }
}
