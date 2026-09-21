//! [`crate::LoadName`]: a validated load-time discriminator.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::segment::{SegmentFault, check_segment};
use crate::Schema;
use crate::schema::{LabelNode, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode};

/// A caller-supplied load name validated against the segment grammar. Unlike
/// [`crate::Namespace`] this is display data the lineage fold has
/// to read, so the text stays reachable through [`as_str`](Self::as_str);
/// unlike a raw string it is valid by construction on every path in,
/// including wire decode and `Deserialize`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct LoadName(Box<str>);

impl LoadName {
    /// Validate `text` as a namespace segment: non-empty, at most 256
    /// bytes, no `:` or `/`, no control or whitespace characters.
    ///
    /// # Errors
    ///
    /// Returns [`LoadNameError`] naming the violated rule when `text`
    /// breaks the grammar.
    pub fn new(text: &str) -> Result<Self, LoadNameError> {
        match check_segment(text.as_bytes()) {
            Ok(()) => Ok(Self(text.into())),
            Err(fault) => Err(LoadNameError(fault)),
        }
    }

    /// The validated text. The lineage fold consumes this when an instanced
    /// node's [`crate::ActorId`] folds the discriminator in.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        &self.0
    }
}

/// [`LoadName::new`] rejection: which segment rule the text broke.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LoadNameError(SegmentFault);

impl fmt::Display for LoadNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid load name: {}", self.0.message())
    }
}

impl StdError for LoadNameError {}

impl Schema for LoadName {
    const SCHEMA: SchemaType = SchemaType::String;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl WireEncode for LoadName {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.as_str().encode(out)
    }
}

impl<'de> WireDecode<'de> for LoadName {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let text = String::decode(cursor)?;
        check_segment(text.as_bytes()).map_err(|_| WireError::InvalidLoadName)?;
        Ok(Self(text.into()))
    }
}

impl Serialize for LoadName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LoadName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <Box<str>>::deserialize(deserializer)?;
        Self::new(&text).map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_invalid_segment() {
        let bytes = [3, 0, 0, 0, b'a', b':', b'b'];
        let mut cursor: &[u8] = &bytes;
        assert_eq!(LoadName::decode(&mut cursor), Err(WireError::InvalidLoadName));
    }
}
