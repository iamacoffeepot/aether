//! 32-byte content-addressed identity of a stored blob.

use alloc::vec::Vec;
use core::fmt;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, LabelNode, Schema, SchemaType, StorageLeaves};

/// 32-byte sha256 of a stored blob.
///
/// A `Digest` field is a citation the store cannot type-check: the expected
/// kind is known only from a `Program` at runtime. The leaf impls delegate to
/// `[u8; 32]` so a digest can sit on a `Storage` event, and [`Cites`] pushes
/// nothing so `append` does not pretend to verify it. Only the driver may
/// write a kind that carries one, and the driver checks both prefixes against
/// the declaration before append. This is the one deliberate bend in the
/// typed-citation rule.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Borrow the 32 digest bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Wrap already-hashed digest bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Schema for Digest {
    const SCHEMA: SchemaType = <[u8; 32] as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Digest"));
    const LABEL_NODE: LabelNode = <[u8; 32] as Schema>::LABEL_NODE;
}

impl StorageLeaves for Digest {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        <[u8; 32] as StorageLeaves>::contribute(&self.0, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self(<[u8; 32] as StorageLeaves>::assemble(carry, depth, source)?))
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        <[u8; 32] as StorageLeaves>::is_absent(carry, depth, source)
    }
}

impl WireEncode for Digest {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for Digest {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Ok(Self(<[u8; 32] as WireDecode>::decode(cursor)?))
    }
}

impl StorageElement for Digest {
    const TAGGED: bool = <[u8; 32] as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.0.contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Ok(Self(<[u8; 32] as StorageElement>::assemble_element(depth, cursor)?))
    }
}

impl Cites for Digest {
    fn cites(&self, _sink: &mut Citations) {}
}
