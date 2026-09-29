//! Unit keys: the key each of a unit's bundle roots is born under (ADR-0240
//! D4), as `aether.bloomery.bundle.<hash>:<unit key>`.

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

/// A `LoadName` of at most [`Self::MAX_BYTES`] bytes: the key of one unit
/// (ADR-0240).
///
/// Fallible on construction, wire decode, and `Deserialize`, so every key in
/// hand can key a bundle root, `aether.bloomery.bundle.<hash>:<unit key>`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct UnitKey(LoadName);

impl UnitKey {
    /// The longest key: one load-name segment, since the key is a bundle
    /// root's whole key segment (ADR-0241 §5).
    pub const MAX_BYTES: usize = aether_actor::NAMESPACE_SEGMENT_MAX_LEN;

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

impl aether_data::CrossesActors for UnitKey {}
impl aether_data::CrossesWire for UnitKey {}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_limit_is_one_load_name_segment() {
        // Tripwire: a bundle root's key is its whole key segment, so the
        // longest key is the longest segment a load name admits.
        assert!(UnitKey::new(&"k".repeat(UnitKey::MAX_BYTES)).is_ok(), "the longest key is accepted");
        assert!(UnitKey::new(&"k".repeat(UnitKey::MAX_BYTES + 1)).is_err(), "a longer key is refused");
    }

    #[test]
    fn decode_refuses_an_over_long_key() {
        let mut bytes = Vec::new();
        "k".repeat(UnitKey::MAX_BYTES + 1).encode(&mut bytes).expect("encode");

        let mut cursor: &[u8] = &bytes;
        assert!(UnitKey::decode(&mut cursor).is_err());
    }
}
