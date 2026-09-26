//! Unit keys and the load names of a unit's bundle roots (ADR-0240 D4).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{LabelNode, LoadName, LoadNameError, Schema, SchemaType};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Digest;

/// A `LoadName` of at most 191 bytes: the key of one unit (ADR-0240).
///
/// Fallible on construction, wire decode, and `Deserialize`, so every key in
/// hand can name a bundle root through [`UnitBundle::name`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct UnitKey(LoadName);

impl UnitKey {
    /// The longest key: one 256-byte path segment, less the dash and the 64
    /// hex characters of the digest that [`UnitBundle::name`] appends.
    pub const MAX_BYTES: usize = 191;

    /// Validate `text` as a load name, then against [`Self::MAX_BYTES`].
    ///
    /// # Errors
    ///
    /// [`UnitKeyError::Name`] when `text` breaks the load-name grammar, and
    /// [`UnitKeyError::TooLong`] when it is longer than [`Self::MAX_BYTES`].
    pub fn new(text: &str) -> Result<Self, UnitKeyError> {
        let name = LoadName::new(text).map_err(UnitKeyError::Name)?;
        if text.len() > Self::MAX_BYTES {
            return Err(UnitKeyError::TooLong { bytes: text.len() });
        }
        Ok(Self(name))
    }

    /// The validated key text.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for UnitKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`UnitKey::new`] rejection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnitKeyError {
    /// The key breaks the load-name grammar.
    Name(LoadNameError),
    /// The key is longer than [`UnitKey::MAX_BYTES`].
    TooLong {
        /// The refused key's length.
        bytes: usize,
    },
}

impl fmt::Display for UnitKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(error) => write!(f, "invalid unit key: {error}"),
            Self::TooLong { bytes } => {
                write!(f, "invalid unit key: {bytes} bytes exceeds the {}-byte limit", UnitKey::MAX_BYTES)
            }
        }
    }
}

impl StdError for UnitKeyError {}

impl Schema for UnitKey {
    const SCHEMA: SchemaType = SchemaType::String;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl WireEncode for UnitKey {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.as_str().encode(out)
    }
}

impl<'de> WireDecode<'de> for UnitKey {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let text = String::decode(cursor)?;
        Self::new(&text).map_err(|error| match error {
            UnitKeyError::Name(_) => WireError::InvalidLoadName,
            UnitKeyError::TooLong { .. } => WireError::Message(format!("aether wire: {error}")),
        })
    }
}

impl Serialize for UnitKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for UnitKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <Box<str>>::deserialize(deserializer)?;
        Self::new(&text).map_err(DeError::custom)
    }
}

/// The load name of a unit's bundle root.
pub struct UnitBundle;

impl UnitBundle {
    /// `<key>-<digest>`, the digest as 64 lowercase hex characters.
    ///
    /// Every bundle root is a child of the one engine-level
    /// `aether.component`, so this name is what keeps two units' roots of
    /// one digest apart and what says, in a log, a trace, or an MCP path,
    /// which unit a root folds for and which bundle it runs (ADR-0240 I-2,
    /// I-8). The digest comes last at a fixed width, so the name splits back
    /// into key and digest even when the key contains dashes; the key's
    /// 191-byte limit keeps the whole name inside one 256-byte segment.
    ///
    /// # Panics
    ///
    /// Never in practice: a key is a valid segment of at most
    /// [`UnitKey::MAX_BYTES`] bytes, and the dash and hex digits keep the
    /// name a valid segment of at most 256 bytes.
    #[must_use]
    pub fn name(key: &UnitKey, digest: &Digest) -> LoadName {
        LoadName::new(&format!("{key}-{digest}")).expect("a unit key and a digest always form a load name")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_limit_is_the_longest_key_that_names_a_bundle_root() {
        // Tripwire: a bundle-root name is the key, a dash, and 64 hex
        // characters inside one 256-byte segment, so the key limit is
        // 256 - 65 = 191.
        let longest = UnitKey::new(&"k".repeat(UnitKey::MAX_BYTES)).expect("the longest key is accepted");
        let name = UnitBundle::name(&longest, &Digest::from_bytes([0; 32]));

        assert_eq!(name.as_str().len(), 256);
        assert_eq!(
            UnitKey::new(&"k".repeat(UnitKey::MAX_BYTES + 1)),
            Err(UnitKeyError::TooLong { bytes: UnitKey::MAX_BYTES + 1 })
        );
    }

    #[test]
    fn decode_refuses_an_over_long_key() {
        let mut bytes = Vec::new();
        "k".repeat(UnitKey::MAX_BYTES + 1).encode(&mut bytes).expect("encode");

        let mut cursor: &[u8] = &bytes;
        assert!(UnitKey::decode(&mut cursor).is_err());
    }

    #[test]
    fn a_bundle_name_splits_back_into_key_and_digest() {
        let digest = Digest::from_bytes([0xab; 32]);
        let name = UnitBundle::name(&UnitKey::new("a-b-c").expect("key"), &digest);

        assert_eq!(name.as_str().rsplit_once('-'), Some(("a-b-c", "ab".repeat(32).as_str())));
        assert_eq!(name.as_str(), format!("a-b-c-{digest}"));
    }
}
