//! Citations of a stored artifact: [`Ref`] with its kind in the type, and
//! [`ErasedRef`] with its kind known only at runtime.

use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use super::{Digest, OpaqueBytes, Utf8Text, artifact_digest};
use crate::storage::{
    RecordReader, RecordWriter, StorageElement, StorageError, assemble_tagged_element, assemble_with_aliases,
    contribute_tagged_element, fold_path_segment,
};
use crate::wire::{Decoder, Encoder, Error as WireError, WireDecode, WireEncode};
use crate::{
    Citations, Cites, Doc, DocCell, DocNode, FieldDoc, Kind, KindId, LabelNode, NamedField, Schema, SchemaType,
    Storage, StorageData, StorageLeaves,
};

/// The only storable citation: 32 bytes, transparent leaf, kind in the type.
pub struct Ref<K> {
    digest: Digest,
    _kind: PhantomData<fn() -> K>,
}

impl<K> Ref<K> {
    /// The digest this citation names.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// Wrap a digest as a citation of `K`. Unchecked; the store verifies the prefix.
    #[must_use]
    pub fn from_digest(digest: Digest) -> Self {
        Self { digest, _kind: PhantomData }
    }
}

impl<K: Kind> Ref<K> {
    /// The same citation with its kind moved from the type to a value.
    #[must_use]
    pub fn erase(self) -> ErasedRef {
        ErasedRef::new(K::ID, self.digest)
    }
}

impl Ref<OpaqueBytes> {
    /// Digest of `payload` stored as [`OpaqueBytes`]. Stages nothing.
    #[must_use]
    pub fn of_bytes(payload: &[u8]) -> Self {
        Self::from_digest(artifact_digest(OpaqueBytes::ID, payload))
    }
}

impl Ref<Utf8Text> {
    /// Digest of `text` stored as [`Utf8Text`]. Stages nothing.
    #[must_use]
    pub fn of_text(text: &str) -> Self {
        Self::from_digest(artifact_digest(Utf8Text::ID, text.as_bytes()))
    }
}

impl<K: Storage + Clone> Ref<K> {
    /// Digest of `value` stored as encoded `K`. Stages nothing.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when encoding fails.
    pub fn of_encoded(value: &K) -> Result<Self, StorageError> {
        let payload = K::encode_storage(&StorageData::from_value(value.clone()))?;
        Ok(Self::from_digest(artifact_digest(K::ID, &payload)))
    }
}

impl<K> Clone for Ref<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for Ref<K> {}

impl<K> PartialEq for Ref<K> {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}

impl<K> Eq for Ref<K> {}

impl<K> Hash for Ref<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.digest.hash(state);
    }
}

impl<K> fmt::Debug for Ref<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ref").field(&self.digest).finish()
    }
}

impl<K: Kind + 'static> Schema for Ref<K> {
    const SCHEMA: SchemaType = <[u8; 32] as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Ref"));
    const LABEL_NODE: LabelNode = <[u8; 32] as Schema>::LABEL_NODE;
}

impl<K> crate::CrossesActors for Ref<K> {}
impl<K> crate::CrossesWire for Ref<K> {}

impl<K: Kind + 'static> StorageLeaves for Ref<K> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        <[u8; 32] as StorageLeaves>::contribute(self.digest.as_bytes(), carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self::from_digest(Digest::from_bytes(<[u8; 32] as StorageLeaves>::assemble(carry, depth, source)?)))
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        <[u8; 32] as StorageLeaves>::is_absent(carry, depth, source)
    }
}

impl<K: Kind + 'static> WireEncode for Ref<K> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.digest.as_bytes().encode(out)
    }
}

impl<'de, K: Kind + 'static> WireDecode<'de> for Ref<K> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Ok(Self::from_digest(Digest::from_bytes(<[u8; 32] as WireDecode>::decode(cursor)?)))
    }
}

impl<K: Kind + 'static> StorageElement for Ref<K> {
    const TAGGED: bool = <[u8; 32] as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.digest.as_bytes().contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Ok(Self::from_digest(Digest::from_bytes(<[u8; 32] as StorageElement>::assemble_element(depth, cursor)?)))
    }
}

impl<K: Kind> Cites for Ref<K> {
    fn cites(&self, sink: &mut Citations) {
        sink.push(K::ID, self.digest.as_bytes());
    }
}

/// A citation whose kind is a value, for a holder that links no Rust type of
/// the cited kind. It cites like [`Ref`]: [`Cites`] pushes its kind and
/// digest, so a closure walk injects the artifact and the store checks its
/// prefix. [`Self::cast`] recovers the typed [`Ref`].
///
/// Its codecs are written by hand because this crate cannot use its own
/// `#[derive(Storage)]`. They are the derive's expansion for a two-field
/// struct `{ kind: KindId, digest: Digest }`: the same field names folded
/// into the path carry, in the same order, with the same element tagging, so
/// an `ErasedRef` stores exactly as a derived struct of that shape does.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ErasedRef {
    /// The kind the cited artifact is stored under.
    kind: KindId,
    /// The digest of the cited artifact.
    digest: Digest,
}

const KIND_FIELD: &str = "kind";
const DIGEST_FIELD: &str = "digest";

impl ErasedRef {
    /// Cite the artifact `digest` names, stored under `kind`. Unchecked, like
    /// [`Ref::from_digest`]; the store verifies the prefix.
    #[must_use]
    pub const fn new(kind: KindId, digest: Digest) -> Self {
        Self { kind, digest }
    }

    /// The kind the cited artifact is stored under.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// The digest this citation names.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// The typed citation, when the cited kind is `K`.
    #[must_use]
    pub fn cast<K: Kind>(self) -> Option<Ref<K>> {
        (self.kind == K::ID).then(|| Ref::from_digest(self.digest))
    }
}

impl Schema for ErasedRef {
    const SCHEMA: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[
            NamedField { name: Cow::Borrowed(KIND_FIELD), ty: <KindId as Schema>::SCHEMA },
            NamedField { name: Cow::Borrowed(DIGEST_FIELD), ty: <Digest as Schema>::SCHEMA },
        ]),
        repr_c: false,
    };
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::ErasedRef"));
    const LABEL_NODE: LabelNode = LabelNode::Struct {
        type_label: Some(Cow::Borrowed(concat!(module_path!(), "::ErasedRef"))),
        field_names: Cow::Borrowed(&[Cow::Borrowed(KIND_FIELD), Cow::Borrowed(DIGEST_FIELD)]),
        fields: Cow::Borrowed(&[<KindId as Schema>::LABEL_NODE, <Digest as Schema>::LABEL_NODE]),
    };
    const DOC_NODE: DocNode = DocNode::Struct {
        fields: Cow::Borrowed(&[
            FieldDoc {
                doc: Doc::Written(Cow::Borrowed("The kind the cited artifact is stored under.")),
                node: DocCell::Static(&<KindId as Schema>::DOC_NODE),
                opaque: "`ErasedRef.kind` has a struct or enum type with no doc tree (a hand-written `Schema`); a program input cannot expose it",
            },
            FieldDoc {
                doc: Doc::Written(Cow::Borrowed("The digest of the cited artifact.")),
                node: DocCell::Static(&<Digest as Schema>::DOC_NODE),
                opaque: "`ErasedRef.digest` has a struct or enum type with no doc tree (a hand-written `Schema`); a program input cannot expose it",
            },
        ]),
    };
}

impl crate::CrossesActors for ErasedRef {}
impl crate::CrossesWire for ErasedRef {}

impl StorageLeaves for ErasedRef {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.kind.contribute(fold_path_segment(carry, KIND_FIELD.as_bytes(), depth), depth + 1, sink)?;
        self.digest.contribute(fold_path_segment(carry, DIGEST_FIELD.as_bytes(), depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        let kind_carry = fold_path_segment(carry, KIND_FIELD.as_bytes(), depth);
        let digest_carry = fold_path_segment(carry, DIGEST_FIELD.as_bytes(), depth);

        Ok(Self {
            kind: assemble_with_aliases::<KindId>(kind_carry, &[], depth + 1, source)?,
            digest: assemble_with_aliases::<Digest>(digest_carry, &[], depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        let kind_absent = <KindId as StorageLeaves>::is_absent(
            fold_path_segment(carry, KIND_FIELD.as_bytes(), depth),
            depth + 1,
            source,
        );
        let digest_absent = <Digest as StorageLeaves>::is_absent(
            fold_path_segment(carry, DIGEST_FIELD.as_bytes(), depth),
            depth + 1,
            source,
        );
        kind_absent && digest_absent
    }
}

impl WireEncode for ErasedRef {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.encode_to(out)
    }

    fn encode_to<E: Encoder + ?Sized>(&self, enc: &mut E) -> Result<(), WireError> {
        self.kind.encode_to(enc)?;
        self.digest.encode_to(enc)
    }
}

impl<'de> WireDecode<'de> for ErasedRef {
    const PROVES_ROUTES: bool =
        <KindId as WireDecode<'de>>::PROVES_ROUTES || <Digest as WireDecode<'de>>::PROVES_ROUTES;

    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::decode_from(cursor)
    }

    fn decode_from<D: Decoder<'de> + ?Sized>(dec: &mut D) -> Result<Self, WireError> {
        Ok(Self { kind: KindId::decode_from(dec)?, digest: Digest::decode_from(dec)? })
    }
}

impl StorageElement for ErasedRef {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl Cites for ErasedRef {
    fn cites(&self, sink: &mut Citations) {
        sink.push(self.kind, self.digest.as_bytes());
    }
}
