//! The name of one entry in a directory. Valid by construction.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, LabelNode, Schema, SchemaType, StorageLeaves};

/// Why [`Name::new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// The string was empty.
    Empty,
    /// The string contained `/` (U+002F).
    Slash,
    /// The string contained NUL (U+0000).
    Nul,
    /// The string was `.` or `..`.
    Dot,
}

impl NameError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Slash => "slash",
            Self::Nul => "nul",
            Self::Dot => "dot",
        }
    }
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for NameError {}

/// One directory entry name. These are our rules, not Unix's: a tree is
/// something models and humans read, so a name that is not text is refused
/// at the boundary. Git's rule that `.git` is not a valid entry name is not
/// adopted here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Name(String);

impl Name {
    /// Accept a name that is non-empty, has no `/` or NUL, and is not `.` or `..`.
    ///
    /// # Errors
    ///
    /// [`NameError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, NameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(NameError::Empty);
        }
        if value.contains('/') {
            return Err(NameError::Slash);
        }
        if value.contains('\0') {
            return Err(NameError::Nul);
        }
        if value == "." || value == ".." {
            return Err(NameError::Dot);
        }
        Ok(Self(value))
    }

    /// Borrow the name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn invariant(error: NameError) -> StorageError {
    StorageError::Invariant { kind: "Name", reason: error.reason() }
}

fn wire_error(error: NameError) -> WireError {
    WireError::Message(error.reason().into())
}

impl Schema for Name {
    const SCHEMA: SchemaType = <String as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Name"));
    const LABEL_NODE: LabelNode = <String as Schema>::LABEL_NODE;
}

impl StorageLeaves for Name {
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

impl WireEncode for Name {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for Name {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::new(String::decode(cursor)?).map_err(wire_error)
    }
}

impl StorageElement for Name {
    const TAGGED: bool = <String as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.0.contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Self::new(String::assemble_element(depth, cursor)?).map_err(invariant)
    }
}

impl Cites for Name {
    fn cites(&self, _sink: &mut Citations) {}
}

#[cfg(test)]
mod tests {
    use super::{Name, NameError};

    #[test]
    fn a_plain_name_is_accepted() {
        assert_eq!(Name::new("Cargo.toml").expect("valid").as_str(), "Cargo.toml");
    }

    #[test]
    fn empty_is_refused() {
        assert_eq!(Name::new("").expect_err("empty"), NameError::Empty);
    }

    #[test]
    fn a_slash_is_refused() {
        assert_eq!(Name::new("a/b").expect_err("slash"), NameError::Slash);
    }

    #[test]
    fn a_nul_is_refused() {
        assert_eq!(Name::new("a\0b").expect_err("nul"), NameError::Nul);
    }

    #[test]
    fn dot_and_dotdot_are_refused() {
        assert_eq!(Name::new(".").expect_err("dot"), NameError::Dot);
        assert_eq!(Name::new("..").expect_err("dotdot"), NameError::Dot);
    }
}
