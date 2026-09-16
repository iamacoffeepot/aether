//! One directory entry: a file, an executable, a symlink, or another tree.

use alloc::borrow::Cow;
use alloc::vec::Vec;

use aether_data::storage::{StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{CastEligible, Citations, Cites, EnumVariant, LabelNode, Schema, SchemaType, VariantLabel};

use crate::tree::path::Path;
use crate::{OpaqueBytes, Ref, Tree};

/// One entry in a [`super::Tree`].
///
/// A map value, so a positional container element (u32 selector, then the
/// variant body). `StorageElement` is written by hand because the derive's
/// positional element decodes through the wire codec, so an invalid [`Path`]
/// would surface as [`StorageError::LeafBody`] of a wire `Message` instead of
/// [`StorageError::Invariant`]; the byte layout itself is pinned by the tree
/// encoding tripwire, which is the only guarantee that matters. `Cites` is
/// written by hand because Schema types do not get an emitted walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File(Ref<OpaqueBytes>),
    Executable(Ref<OpaqueBytes>),
    Symlink(Path),
    Directory(Ref<Tree>),
}

impl Cites for Node {
    fn cites(&self, sink: &mut Citations) {
        match self {
            Self::File(digest) | Self::Executable(digest) => digest.cites(sink),
            Self::Directory(digest) => digest.cites(sink),
            Self::Symlink(_) => {}
        }
    }
}

impl Schema for Node {
    const SCHEMA: SchemaType = SchemaType::Enum {
        variants: Cow::Borrowed(&[
            EnumVariant::Tuple {
                name: Cow::Borrowed("File"),
                discriminant: 0,
                fields: Cow::Borrowed(&[<Ref<OpaqueBytes> as Schema>::SCHEMA]),
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Executable"),
                discriminant: 1,
                fields: Cow::Borrowed(&[<Ref<OpaqueBytes> as Schema>::SCHEMA]),
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Symlink"),
                discriminant: 2,
                fields: Cow::Borrowed(&[<Path as Schema>::SCHEMA]),
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Directory"),
                discriminant: 3,
                fields: Cow::Borrowed(&[<Ref<Tree> as Schema>::SCHEMA]),
            },
        ]),
    };
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Node"));
    const LABEL_NODE: LabelNode = LabelNode::Enum {
        type_label: Some(Cow::Borrowed(concat!(module_path!(), "::Node"))),
        variants: Cow::Borrowed(&[
            VariantLabel::Tuple {
                name: Cow::Borrowed("File"),
                fields: Cow::Borrowed(&[<Ref<OpaqueBytes> as Schema>::LABEL_NODE]),
            },
            VariantLabel::Tuple {
                name: Cow::Borrowed("Executable"),
                fields: Cow::Borrowed(&[<Ref<OpaqueBytes> as Schema>::LABEL_NODE]),
            },
            VariantLabel::Tuple {
                name: Cow::Borrowed("Symlink"),
                fields: Cow::Borrowed(&[<Path as Schema>::LABEL_NODE]),
            },
            VariantLabel::Tuple {
                name: Cow::Borrowed("Directory"),
                fields: Cow::Borrowed(&[<Ref<Tree> as Schema>::LABEL_NODE]),
            },
        ]),
    };
}

impl CastEligible for Node {
    const ELIGIBLE: bool = false;
}

impl WireEncode for Node {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        match self {
            Self::File(digest) => {
                0u32.encode(out)?;
                digest.encode(out)
            }
            Self::Executable(digest) => {
                1u32.encode(out)?;
                digest.encode(out)
            }
            Self::Symlink(target) => {
                2u32.encode(out)?;
                target.encode(out)
            }
            Self::Directory(digest) => {
                3u32.encode(out)?;
                digest.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for Node {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        match u32::decode(cursor)? {
            0 => Ok(Self::File(Ref::decode(cursor)?)),
            1 => Ok(Self::Executable(Ref::decode(cursor)?)),
            2 => Ok(Self::Symlink(Path::decode(cursor)?)),
            3 => Ok(Self::Directory(Ref::decode(cursor)?)),
            other => Err(WireError::InvalidEnum(other)),
        }
    }
}

impl StorageElement for Node {
    const TAGGED: bool = false;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        match self {
            Self::File(digest) => {
                0u32.contribute_element(depth, out)?;
                digest.contribute_element(depth, out)
            }
            Self::Executable(digest) => {
                1u32.contribute_element(depth, out)?;
                digest.contribute_element(depth, out)
            }
            Self::Symlink(target) => {
                2u32.contribute_element(depth, out)?;
                target.contribute_element(depth, out)
            }
            Self::Directory(digest) => {
                3u32.contribute_element(depth, out)?;
                digest.contribute_element(depth, out)
            }
        }
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        match u32::assemble_element(depth, cursor)? {
            0 => Ok(Self::File(Ref::assemble_element(depth, cursor)?)),
            1 => Ok(Self::Executable(Ref::assemble_element(depth, cursor)?)),
            2 => Ok(Self::Symlink(Path::assemble_element(depth, cursor)?)),
            3 => Ok(Self::Directory(Ref::assemble_element(depth, cursor)?)),
            other => Err(StorageError::LeafBody(WireError::InvalidEnum(other))),
        }
    }
}
