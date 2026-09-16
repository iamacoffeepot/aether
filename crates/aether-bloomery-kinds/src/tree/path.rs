//! Inline symlink target. Valid by construction.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, LabelNode, Schema, SchemaType, StorageLeaves};

/// Why [`Path::new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The string was empty.
    Empty,
    /// The string contained NUL (U+0000).
    Nul,
}

impl PathError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Nul => "nul",
        }
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for PathError {}

/// A symlink target stored inline in the tree, not as a separate blob.
/// May contain `/` and `..`; that is what a link target is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(String);

impl Path {
    /// Accept a target that is non-empty and has no NUL.
    ///
    /// # Errors
    ///
    /// [`PathError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, PathError> {
        let value = value.into();
        if value.is_empty() {
            return Err(PathError::Empty);
        }
        if value.contains('\0') {
            return Err(PathError::Nul);
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
    use super::{Path, PathError};

    #[test]
    fn a_relative_target_is_accepted() {
        assert_eq!(Path::new("../bin/run").expect("valid").as_str(), "../bin/run");
    }

    #[test]
    fn empty_is_refused() {
        assert_eq!(Path::new("").expect_err("empty"), PathError::Empty);
    }

    #[test]
    fn a_nul_is_refused() {
        assert_eq!(Path::new("a\0b").expect_err("nul"), PathError::Nul);
    }
}
