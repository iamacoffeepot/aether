//! Canonical directory map. Valid by construction: no case-fold collision.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, LabelNode, Schema, SchemaType, StorageLeaves};

use crate::tree::name::Name;
use crate::tree::node::Node;

/// Why [`super::Tree::new`] refused a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeError {
    /// Two names that are distinct as map keys collide after NFC plus case folding.
    Collides { a: Name, b: Name },
}

impl TreeError {
    const fn reason(&self) -> &'static str {
        match self {
            Self::Collides { .. } => "collides",
        }
    }
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collides { a, b } => write!(f, "collides: {} and {}", a.as_str(), b.as_str()),
        }
    }
}

impl StdError for TreeError {}

/// [`BTreeMap`] of valid names that also refuses a case-insensitive collision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entries(BTreeMap<Name, Node>);

impl Entries {
    pub(super) fn new(map: BTreeMap<Name, Node>) -> Result<Self, TreeError> {
        if let Some((a, b)) = first_collision(&map) {
            return Err(TreeError::Collides { a, b });
        }
        Ok(Self(map))
    }

    pub(super) fn empty() -> Self {
        Self(BTreeMap::new())
    }

    pub(super) fn as_map(&self) -> &BTreeMap<Name, Node> {
        &self.0
    }
}

fn fold_key(name: &Name) -> String {
    name.as_str().chars().flat_map(char::to_lowercase).collect()
}

fn first_collision(map: &BTreeMap<Name, Node>) -> Option<(Name, Name)> {
    let mut seen: BTreeMap<String, Name> = BTreeMap::new();
    for name in map.keys() {
        let folded = fold_key(name);
        if let Some(prior) = seen.get(&folded) {
            return Some((prior.clone(), name.clone()));
        }
        seen.insert(folded, name.clone());
    }
    None
}

fn invariant(error: &TreeError) -> StorageError {
    StorageError::Invariant { kind: "Entries", reason: error.reason() }
}

fn wire_error(error: &TreeError) -> WireError {
    WireError::Message(error.reason().into())
}

type Inner = BTreeMap<Name, Node>;

impl Schema for Entries {
    const SCHEMA: SchemaType = <Inner as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = <Inner as Schema>::LABEL;
    const LABEL_NODE: LabelNode = <Inner as Schema>::LABEL_NODE;
}

impl StorageLeaves for Entries {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.0.contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Self::new(Inner::assemble(carry, depth, source)?).map_err(|error| invariant(&error))
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        Inner::is_absent(carry, depth, source)
    }
}

impl StorageElement for Entries {
    const TAGGED: bool = <Inner as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.0.contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Self::new(Inner::assemble_element(depth, cursor)?).map_err(|error| invariant(&error))
    }
}

impl WireEncode for Entries {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for Entries {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::new(Inner::decode(cursor)?).map_err(|error| wire_error(&error))
    }
}

impl Cites for Entries {
    fn cites(&self, sink: &mut Citations) {
        self.0.cites(sink);
    }
}
