//! The one input every tool program takes: the tree the call works on and
//! the arguments the model wrote.
//!
//! One stored kind, `bloomery.program.tooled`, has two views. The loop that
//! runs a call links no tool's types, so it builds [`ErasedTooled`] with
//! [`tooled`], its arguments' kind a value. The tool reads the same bytes as
//! [`Tooled<A>`], whose arguments' kind is in the type, and a stored value
//! whose arguments are not an `A` refuses to decode as one. The model never
//! sees the envelope: [`ToolArguments`] names `A`, and
//! [`crate::tool_definition`] renders only `A`.

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

/// A tool call's input as the loop that runs it writes it: the session's
/// current tree and the call's decoded arguments, whose kind is known only
/// at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.tooled")]
pub struct ErasedTooled {
    /// The tree the call works on: the session's latest.
    tree: Ref<Tree>,
    /// The call's arguments, stored under the tool's arguments kind.
    args: ErasedRef,
}

impl ErasedTooled {
    /// The tree the call works on.
    #[must_use]
    pub const fn tree(&self) -> Ref<Tree> {
        self.tree
    }

    /// The call's arguments.
    #[must_use]
    pub const fn args(&self) -> ErasedRef {
        self.args
    }
}

/// The input of a call that works on `tree` with the arguments `args`: the
/// one way to build a tool's input without linking the tool.
#[must_use]
pub const fn tooled(tree: Ref<Tree>, args: ErasedRef) -> ErasedTooled {
    ErasedTooled { tree, args }
}

/// A tool program's input: the tree the call works on and its arguments,
/// an `A`. Stored exactly as the [`ErasedTooled`] the loop wrote.
pub struct Tooled<A> {
    tree: Ref<Tree>,
    args: Ref<A>,
}

impl<A> Tooled<A> {
    /// The tree the call works on.
    #[must_use]
    pub const fn tree(&self) -> Ref<Tree> {
        self.tree
    }

    /// The call's arguments.
    #[must_use]
    pub const fn args(&self) -> Ref<A> {
        self.args
    }
}

impl<A: Kind> Tooled<A> {
    /// The same input with its arguments' kind moved from the type to a
    /// value.
    #[must_use]
    pub fn erase(&self) -> ErasedTooled {
        tooled(self.tree, self.args.erase())
    }

    fn from_erased(erased: ErasedTooled) -> Result<Self, StorageError> {
        let args = erased
            .args
            .cast::<A>()
            .ok_or_else(|| StorageError::TypeMismatch { expected: A::ID, actual: erased.args.kind() })?;
        Ok(Self { tree: erased.tree, args })
    }
}

impl<A> Clone for Tooled<A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A> Copy for Tooled<A> {}

impl<A> PartialEq for Tooled<A> {
    fn eq(&self, other: &Self) -> bool {
        self.tree == other.tree && self.args == other.args
    }
}

impl<A> Eq for Tooled<A> {}

impl<A> fmt::Debug for Tooled<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tooled").field("tree", &self.tree).field("args", &self.args).finish()
    }
}

mod sealed {
    pub trait Sealed {}
}

impl<A> sealed::Sealed for Tooled<A> {}

/// A program input that makes its program a tool: only [`Tooled<A>`], whose
/// arguments `A` are what the model writes.
pub trait ToolArguments: sealed::Sealed {
    /// What the model writes for a call: the arguments the tool reads.
    type Arguments: Storage + Schema + Clone + Cites + Send + 'static;
}

impl<A: Storage + Schema + Clone + Cites + Send + 'static> ToolArguments for Tooled<A> {
    type Arguments = A;
}

fn wire_error(error: &StorageError) -> WireError {
    WireError::Message(alloc::format!("{error}"))
}

impl<A> Schema for Tooled<A> {
    const SCHEMA: SchemaType = <ErasedTooled as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Tooled"));
    const LABEL_NODE: LabelNode = <ErasedTooled as Schema>::LABEL_NODE;
    const DOC_NODE: DocNode = <ErasedTooled as Schema>::DOC_NODE;
}

impl<A> aether_data::CrossesActors for Tooled<A> {}
impl<A> aether_data::CrossesWire for Tooled<A> {}

impl<A: Kind + 'static> Kind for Tooled<A> {
    const NAME: &'static str = <ErasedTooled as Kind>::NAME;
    const ID: KindId = <ErasedTooled as Kind>::ID;

    fn encode_into_bytes(&self) -> Vec<u8> {
        panic!(
            "aether-data: Kind::encode_into_bytes called on storage kind `{}`. Storage values do not have a \
             positional mail codec; they reach mail only through handle indirection.",
            Self::NAME
        )
    }
}

impl<A: Kind + 'static> StorageLeaves for Tooled<A> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.erase().contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Self::from_erased(ErasedTooled::assemble(carry, depth, source)?)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        ErasedTooled::is_absent(carry, depth, source)
    }
}

impl<A: Kind + 'static> Storage for Tooled<A> {
    fn decode_storage(bytes: &[u8]) -> Result<StorageData<Self>, StorageError> {
        decode_derived(bytes, Self::STRICT)
    }

    fn encode_storage(data: &StorageData<Self>) -> Result<Vec<u8>, StorageError> {
        encode_derived(data)
    }
}

impl<A: Kind + 'static> StorageElement for Tooled<A> {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl<A: Kind + 'static> WireEncode for Tooled<A> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.erase().encode(out)
    }
}

impl<'de, A: Kind + 'static> WireDecode<'de> for Tooled<A> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::from_erased(ErasedTooled::decode(cursor)?).map_err(|error| wire_error(&error))
    }
}

impl<A: Kind> Cites for Tooled<A> {
    fn cites(&self, sink: &mut Citations) {
        self.tree.cites(sink);
        self.args.cites(sink);
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ClosureArtifact, EncodedArtifact, ErasedRef, Ref, Tree};
    use aether_data::{Kind, Storage, StorageError};

    use super::{Tooled, tooled};
    use crate::ToolSchema;

    #[test]
    fn the_loops_erased_input_is_the_tools_typed_input_and_a_foreign_argument_kind_refuses() {
        // Catches a hand-written codec or citation walk that drifts from the erased form the loop encodes, which
        // would make the tool's input artifact differ from the loop's, and a dropped check on the arguments' kind.
        let tree = Ref::<Tree>::of_encoded(&Tree::empty()).expect("tree");
        let args = Ref::<ToolSchema>::of_encoded(&ToolSchema::of::<Tree>()).expect("args");
        let erased = EncodedArtifact::new(&tooled(tree, args.erase())).expect("erased");
        let (kind, payload, citations) = erased.clone().into_parts();
        let payload = ClosureArtifact::new(kind, payload).load(erased.digest()).expect("staged bytes hash");

        let typed = Tooled::<ToolSchema>::decode_storage(&payload).expect("the erased input decodes typed").value;
        assert_eq!((typed.tree(), typed.args()), (tree, args));
        let reencoded = EncodedArtifact::new(&typed).expect("typed");
        assert_eq!(reencoded.digest(), erased.digest(), "one stored kind, one payload");
        assert_eq!(reencoded.into_parts().2, citations, "the same citations, in order");
        assert_eq!(kind, Tooled::<ToolSchema>::ID);

        let foreign = EncodedArtifact::new(&tooled(tree, ErasedRef::new(Tree::ID, args.digest()))).expect("foreign");
        let digest = foreign.digest();
        let (kind, payload, _) = foreign.into_parts();
        let payload = ClosureArtifact::new(kind, payload).load(digest).expect("staged bytes hash");
        assert!(matches!(
            Tooled::<ToolSchema>::decode_storage(&payload),
            Err(StorageError::TypeMismatch { expected, actual }) if expected == ToolSchema::ID && actual == Tree::ID
        ));
    }
}
