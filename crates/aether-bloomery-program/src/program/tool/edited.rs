//! What a tool that changes the tree returns.
//!
//! One stored kind, `bloomery.program.edited`, has two views, as
//! `bloomery.program.tooled` does. The loop that runs a call links no tool's
//! types, so it reads a result as [`ErasedEdited`], its detail's kind a
//! value, and takes its tree. The tool writes the same bytes as
//! [`Edited<D>`], whose detail's kind is in the type, and a stored value whose
//! detail is of another kind refuses to decode as one. A tool with nothing
//! more to say than its summary returns `Edited<NoDetail>`, written `Edited`.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use aether_bloomery_kinds::{ErasedRef, Ref, Tree};
use aether_data::storage::{
    RecordReader, RecordWriter, StorageElement, assemble_tagged_element, contribute_tagged_element, decode_derived,
    encode_derived,
};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{
    Citations, Cites, DocNode, Kind, KindId, LabelNode, Schema, SchemaType, Storage, StorageData, StorageError,
    StorageLeaves,
};

/// A tree-changing tool's result as the loop that runs the call reads it:
/// the tree after the call, what it did, and its detail, whose kind is known
/// only at runtime.
///
/// The loop binds the tree into every later call. A call that changed nothing
/// returns the tree it was given, and its summary says why.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.edited")]
pub struct ErasedEdited {
    /// The tree after the call.
    tree: Ref<Tree>,
    /// What the call did, for the model to read: one sentence on what it
    /// changed or why it changed nothing, then any text the call reports.
    summary: String,
    /// What else the call found, stored under the tool's detail kind.
    detail: ErasedRef,
}

impl ErasedEdited {
    /// The tree after the call.
    #[must_use]
    pub const fn tree(&self) -> Ref<Tree> {
        self.tree
    }

    /// What the call did.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// What else the call found.
    #[must_use]
    pub const fn detail(&self) -> ErasedRef {
        self.detail
    }
}

/// The detail of a tool that has nothing more to say than its summary: the
/// detail of `Edited<NoDetail>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.no_detail")]
pub struct NoDetail;

/// A tree-changing tool's result: the tree after the call, what it did, and
/// its detail `D`. Stored exactly as the [`ErasedEdited`] the loop reads.
///
/// The detail is a citation, so the tool stages the value it cites; a
/// citation the store does not hold refuses the run.
pub struct Edited<D = NoDetail> {
    tree: Ref<Tree>,
    summary: String,
    detail: Ref<D>,
}

impl<D> Edited<D> {
    /// The call left `tree`, `summary` says what it did, and `detail` cites
    /// what else it found.
    #[must_use]
    pub fn new(tree: Ref<Tree>, summary: impl Into<String>, detail: Ref<D>) -> Self {
        Self { tree, summary: summary.into(), detail }
    }

    /// The tree after the call.
    #[must_use]
    pub const fn tree(&self) -> Ref<Tree> {
        self.tree
    }

    /// What the call did.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// What else the call found.
    #[must_use]
    pub const fn detail(&self) -> Ref<D> {
        self.detail
    }
}

impl<D: Kind> Edited<D> {
    /// The same result with its detail's kind moved from the type to a value.
    #[must_use]
    pub fn erase(&self) -> ErasedEdited {
        ErasedEdited { tree: self.tree, summary: self.summary.clone(), detail: self.detail.erase() }
    }

    fn from_erased(erased: ErasedEdited) -> Result<Self, StorageError> {
        let detail = erased
            .detail
            .cast::<D>()
            .ok_or_else(|| StorageError::TypeMismatch { expected: D::ID, actual: erased.detail.kind() })?;
        Ok(Self { tree: erased.tree, summary: erased.summary, detail })
    }
}

impl<D> Clone for Edited<D> {
    fn clone(&self) -> Self {
        Self { tree: self.tree, summary: self.summary.clone(), detail: self.detail }
    }
}

impl<D> PartialEq for Edited<D> {
    fn eq(&self, other: &Self) -> bool {
        self.tree == other.tree && self.summary == other.summary && self.detail == other.detail
    }
}

impl<D> Eq for Edited<D> {}

impl<D> fmt::Debug for Edited<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Edited")
            .field("tree", &self.tree)
            .field("summary", &self.summary)
            .field("detail", &self.detail)
            .finish()
    }
}

fn wire_error(error: &StorageError) -> WireError {
    WireError::Message(alloc::format!("{error}"))
}

impl<D> Schema for Edited<D> {
    const SCHEMA: SchemaType = <ErasedEdited as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Edited"));
    const LABEL_NODE: LabelNode = <ErasedEdited as Schema>::LABEL_NODE;
    const DOC_NODE: DocNode = <ErasedEdited as Schema>::DOC_NODE;
}

impl<D> aether_data::CrossesActors for Edited<D> {}
impl<D> aether_data::CrossesWire for Edited<D> {}

impl<D: Kind + 'static> Kind for Edited<D> {
    const NAME: &'static str = <ErasedEdited as Kind>::NAME;
    const ID: KindId = <ErasedEdited as Kind>::ID;

    fn encode_into_bytes(&self) -> Vec<u8> {
        panic!(
            "aether-data: Kind::encode_into_bytes called on storage kind `{}`. Storage values do not have a \
             positional mail codec; they reach mail only through handle indirection.",
            Self::NAME
        )
    }
}

impl<D: Kind + 'static> StorageLeaves for Edited<D> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.erase().contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Self::from_erased(ErasedEdited::assemble(carry, depth, source)?)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        ErasedEdited::is_absent(carry, depth, source)
    }
}

impl<D: Kind + 'static> Storage for Edited<D> {
    fn decode_storage(bytes: &[u8]) -> Result<StorageData<Self>, StorageError> {
        decode_derived(bytes, Self::STRICT)
    }

    fn encode_storage(data: &StorageData<Self>) -> Result<Vec<u8>, StorageError> {
        encode_derived(data)
    }
}

impl<D: Kind + 'static> StorageElement for Edited<D> {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl<D: Kind + 'static> WireEncode for Edited<D> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.erase().encode(out)
    }
}

impl<'de, D: Kind + 'static> WireDecode<'de> for Edited<D> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::from_erased(ErasedEdited::decode(cursor)?).map_err(|error| wire_error(&error))
    }
}

impl<D: Kind> Cites for Edited<D> {
    fn cites(&self, sink: &mut Citations) {
        self.tree.cites(sink);
        self.detail.cites(sink);
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ClosureArtifact, EncodedArtifact, ErasedRef, Ref, Tree};
    use aether_data::{Kind, Storage, StorageError};

    use super::{Edited, ErasedEdited, NoDetail};

    #[test]
    fn the_loops_erased_result_is_the_tools_typed_result_and_a_foreign_detail_kind_refuses() {
        // Catches a hand-written codec or citation walk that drifts from the erased form the loop reads, which would
        // make the tool's stored result differ from what the loop decodes, a dropped check on the detail's kind, or
        // a cites walk that omits the detail.
        let tree = Ref::<Tree>::of_encoded(&Tree::empty()).expect("tree");
        let detail = Ref::<NoDetail>::of_encoded(&NoDetail).expect("detail");
        let typed = EncodedArtifact::new(&Edited::new(tree, "Edited path.", detail)).expect("typed");
        let (kind, payload, citations) = typed.clone().into_parts();
        let payload = ClosureArtifact::new(kind, payload).load(typed.digest()).expect("staged bytes hash");

        let erased = ErasedEdited::decode_storage(&payload).expect("the typed result decodes erased").value;
        assert_eq!((erased.tree(), erased.summary(), erased.detail()), (tree, "Edited path.", detail.erase()));
        let reencoded = EncodedArtifact::new(&erased).expect("erased");
        assert_eq!(reencoded.digest(), typed.digest(), "one stored kind, one payload");
        assert_eq!(reencoded.into_parts().2, citations, "the same citations, in order");
        assert_eq!(kind, Edited::<NoDetail>::ID);

        let foreign =
            ErasedEdited { tree, summary: "Edited path.".into(), detail: ErasedRef::new(Tree::ID, detail.digest()) };
        let foreign = EncodedArtifact::new(&foreign).expect("foreign");
        let digest = foreign.digest();
        let (kind, payload, _) = foreign.into_parts();
        let payload = ClosureArtifact::new(kind, payload).load(digest).expect("staged bytes hash");
        assert!(matches!(
            Edited::<NoDetail>::decode_storage(&payload),
            Err(StorageError::TypeMismatch { expected, actual }) if expected == NoDetail::ID && actual == Tree::ID
        ));
    }
}
