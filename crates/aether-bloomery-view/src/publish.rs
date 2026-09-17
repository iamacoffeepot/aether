//! Opt-in owned snapshots of a [`View`]. Not every view is [`Publish`].

use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry as BindingEntry;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{Digest, HeadNameError, RecordedHead, Seq};
use aether_data::canonical::{canonical_len_kind, canonical_serialize_kind};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode, decode_from_slice, encode_to_vec};
use aether_data::{KIND_DOMAIN, Kind, KindId, LabelNode, Schema, SchemaType, Tag, fnv1a_64_prefixed, with_tag};

use crate::heads::Heads;
use crate::view::View;

const HEADS_NAME: &str = "bloomery.view.heads";

/// Owned same-type snapshot of a view. Uses the structured mail codec, not
/// journal [`aether_data::Storage`] bytes.
pub trait Publish: View + Sized + 'static {
    /// Independent owned copy of this view at its current cursor.
    #[must_use]
    fn snapshot(&self) -> Self;

    /// Encode an owned snapshot.
    ///
    /// # Errors
    ///
    /// [`PublishError::Wire`] when the snapshot exceeds the `u32` length ceiling.
    fn encode(&self) -> Result<Vec<u8>, PublishError>;

    /// Decode an owned snapshot of this type.
    ///
    /// # Errors
    ///
    /// [`PublishError`] when the bytes are not a valid snapshot of `Self`.
    fn decode(bytes: &[u8]) -> Result<Self, PublishError>;
}

/// Failure to encode or decode a published view.
#[derive(Debug)]
pub enum PublishError {
    /// Structured wire codec failure.
    Wire(WireError),
    /// Two bindings named the same recorded head.
    Duplicate(RecordedHead),
    /// Cursor `Seq(0)` cannot carry bindings.
    InconsistentCursor {
        /// Cursor in the snapshot.
        cursor: Seq,
        /// Number of bindings in the snapshot.
        bindings: usize,
    },
    /// A binding's head name failed the recorded-head rules.
    InvalidHead(HeadNameError),
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => write!(f, "{error}"),
            Self::Duplicate(head) => {
                write!(f, "published heads snapshot repeats head {} ({})", head.as_str(), head.kind())
            }
            Self::InconsistentCursor { cursor, bindings } => {
                write!(f, "published heads snapshot at {cursor} carries {bindings} bindings")
            }
            Self::InvalidHead(error) => write!(f, "published heads snapshot has an invalid head name: {error}"),
        }
    }
}

impl Error for PublishError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Wire(error) => Some(error),
            Self::InvalidHead(error) => Some(error),
            Self::Duplicate(_) | Self::InconsistentCursor { .. } => None,
        }
    }
}

impl From<WireError> for PublishError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

impl From<HeadNameError> for PublishError {
    fn from(error: HeadNameError) -> Self {
        Self::InvalidHead(error)
    }
}

#[derive(Clone, Debug, aether_data::Schema)]
struct HeadsSnapshot {
    cursor: u64,
    bindings: Vec<PublishedBinding>,
}

#[derive(Clone, Debug, aether_data::Schema)]
struct PublishedBinding {
    target_kind: KindId,
    head: String,
    to: Digest,
}

static HEADS_SCHEMA: SchemaType = <HeadsSnapshot as Schema>::SCHEMA;
const HEADS_CANONICAL_LEN: usize = canonical_len_kind(HEADS_NAME, &HEADS_SCHEMA);
const HEADS_CANONICAL_BYTES: [u8; HEADS_CANONICAL_LEN] = canonical_serialize_kind(HEADS_NAME, &HEADS_SCHEMA);

impl From<&Heads> for HeadsSnapshot {
    fn from(heads: &Heads) -> Self {
        Self {
            cursor: heads.cursor().0,
            bindings: heads
                .bindings()
                .iter()
                .map(|(head, to)| PublishedBinding {
                    target_kind: head.kind(),
                    head: String::from(head.as_str()),
                    to: *to,
                })
                .collect(),
        }
    }
}

impl TryFrom<HeadsSnapshot> for Heads {
    type Error = PublishError;

    fn try_from(snapshot: HeadsSnapshot) -> Result<Self, Self::Error> {
        let cursor = Seq(snapshot.cursor);
        let mut bindings = BTreeMap::new();
        for binding in snapshot.bindings {
            match bindings.entry(RecordedHead::new(binding.target_kind, binding.head)?) {
                BindingEntry::Vacant(slot) => {
                    slot.insert(binding.to);
                }
                BindingEntry::Occupied(slot) => return Err(PublishError::Duplicate(slot.key().clone())),
            }
        }
        if cursor == Seq(0) && !bindings.is_empty() {
            return Err(PublishError::InconsistentCursor { cursor, bindings: bindings.len() });
        }
        Ok(Self::reconstruct(cursor, bindings))
    }
}

impl Schema for Heads {
    const SCHEMA: SchemaType = <HeadsSnapshot as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Heads"));
    const LABEL_NODE: LabelNode = <HeadsSnapshot as Schema>::LABEL_NODE;
}

impl Kind for Heads {
    const NAME: &'static str = HEADS_NAME;
    const ID: KindId = KindId(with_tag(Tag::Kind, fnv1a_64_prefixed(KIND_DOMAIN, &HEADS_CANONICAL_BYTES)));

    fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
        <Self as Publish>::decode(bytes).ok()
    }

    fn encode_into_bytes(&self) -> Vec<u8> {
        encode_to_vec(self).expect("wire encode to Vec fails only past the u32 length ceiling")
    }
}

impl WireEncode for Heads {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        HeadsSnapshot::from(self).encode(out)
    }
}

impl<'de> WireDecode<'de> for Heads {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::try_from(HeadsSnapshot::decode(cursor)?).map_err(|error| WireError::Message(alloc::format!("{error}")))
    }
}

impl Publish for Heads {
    fn snapshot(&self) -> Self {
        self.clone()
    }

    fn encode(&self) -> Result<Vec<u8>, PublishError> {
        Ok(encode_to_vec(self)?)
    }

    fn decode(bytes: &[u8]) -> Result<Self, PublishError> {
        let snapshot = decode_from_slice::<HeadsSnapshot>(bytes)?;
        Self::try_from(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::{HeadsSnapshot, Publish, PublishError, PublishedBinding};
    use crate::Heads;
    use aether_bloomery_kinds::{Digest, RecordedHead};
    use aether_data::{Kind, KindId, wire};
    use alloc::string::String;
    use alloc::vec;

    fn program_head(name: &str) -> RecordedHead {
        RecordedHead::new(aether_bloomery_kinds::Program::ID, name).expect("valid head")
    }

    #[test]
    fn duplicate_bindings_are_refused() {
        // Bug: a snapshot that lists the same head twice decodes as last-write-wins.
        let head = program_head("main");
        let snapshot = HeadsSnapshot {
            cursor: 2,
            bindings: vec![
                PublishedBinding {
                    target_kind: head.kind(),
                    head: String::from(head.as_str()),
                    to: Digest::from_bytes([1; 32]),
                },
                PublishedBinding {
                    target_kind: head.kind(),
                    head: String::from(head.as_str()),
                    to: Digest::from_bytes([2; 32]),
                },
            ],
        };
        let bytes = wire::encode_to_vec(&snapshot).expect("snapshot encodes");
        let error = Heads::decode(&bytes).expect_err("duplicate bindings");
        assert!(matches!(error, PublishError::Duplicate(ref actual) if actual == &head), "{error}");
    }

    #[test]
    fn empty_cursor_cannot_carry_bindings() {
        // Bug: Seq(0) with a binding claims an empty prefix that already moved a head.
        let snapshot = HeadsSnapshot {
            cursor: 0,
            bindings: vec![PublishedBinding {
                target_kind: aether_bloomery_kinds::Program::ID,
                head: String::from("main"),
                to: Digest::from_bytes([1; 32]),
            }],
        };
        let bytes = wire::encode_to_vec(&snapshot).expect("snapshot encodes");
        let error = Heads::decode(&bytes).expect_err("inconsistent cursor");
        assert!(matches!(error, PublishError::InconsistentCursor { cursor, bindings: 1 } if cursor.0 == 0), "{error}");
    }

    #[test]
    fn invalid_head_names_are_refused() {
        // Bug: a published binding with an empty name becomes a Heads lookup key.
        let snapshot = HeadsSnapshot {
            cursor: 1,
            bindings: vec![PublishedBinding {
                target_kind: KindId(1),
                head: String::new(),
                to: Digest::from_bytes([1; 32]),
            }],
        };
        let bytes = wire::encode_to_vec(&snapshot).expect("snapshot encodes");
        let error = Heads::decode(&bytes).expect_err("invalid head");
        assert!(matches!(error, PublishError::InvalidHead(_)), "{error}");
    }
}
