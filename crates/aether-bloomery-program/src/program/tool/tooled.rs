//! The one input every tool program takes: the tree the call works on, the
//! arguments the model wrote, and the session values the loop binds.
//!
//! One stored kind, `bloomery.program.tooled`, has two views. The loop that
//! runs a call links no tool's types, so it builds [`ErasedTooled`] with
//! [`tooled`], its arguments' and bound's kinds as values. The tool reads the
//! same bytes as [`Tooled<A, B>`], whose arguments' and bound's kinds are in
//! the type, and a stored value whose arguments or bound are of another kind
//! refuses to decode as one. The model sees only `A`: [`ToolArguments`]
//! names `A` and `B`, and [`crate::tool_definition`] renders only `A`. A tool
//! with no extra values takes `Tooled<A, NoBound>`, written `Tooled<A>`.

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
/// current tree, the call's decoded arguments, and the per-tool session
/// values, whose kinds are known only at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.tooled")]
pub struct ErasedTooled {
    /// The tree the call works on: the session's latest.
    tree: Ref<Tree>,
    /// The call's arguments, stored under the tool's arguments kind.
    args: ErasedRef,
    /// The per-tool session values, stored under the tool's bound kind.
    bound: ErasedRef,
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

    /// The per-tool session values the loop bound.
    #[must_use]
    pub const fn bound(&self) -> ErasedRef {
        self.bound
    }
}

/// The input of a call that works on `tree` with the arguments `args` and the
/// per-tool session values `bound`: the one way to build a tool's input
/// without linking the tool.
#[must_use]
pub const fn tooled(tree: Ref<Tree>, args: ErasedRef, bound: ErasedRef) -> ErasedTooled {
    ErasedTooled { tree, args, bound }
}

/// A tool with no extra session values: the bound of `Tooled<A, NoBound>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.no_bound")]
pub struct NoBound;

/// A tool program's input: the tree the call works on, its arguments `A`,
/// and its session-bound values `B`. Stored exactly as the [`ErasedTooled`]
/// the loop wrote.
pub struct Tooled<A, B = NoBound> {
    tree: Ref<Tree>,
    args: Ref<A>,
    bound: Ref<B>,
}

impl<A, B> Tooled<A, B> {
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

    /// The per-tool session values the loop bound.
    #[must_use]
    pub const fn bound(&self) -> Ref<B> {
        self.bound
    }
}

impl<A: Kind, B: Kind> Tooled<A, B> {
    /// The same input with its arguments' and bound's kinds moved from the
    /// type to values.
    #[must_use]
    pub fn erase(&self) -> ErasedTooled {
        tooled(self.tree, self.args.erase(), self.bound.erase())
    }

    fn from_erased(erased: ErasedTooled) -> Result<Self, StorageError> {
        let args = erased
            .args
            .cast::<A>()
            .ok_or_else(|| StorageError::TypeMismatch { expected: A::ID, actual: erased.args.kind() })?;
        let bound = erased
            .bound
            .cast::<B>()
            .ok_or_else(|| StorageError::TypeMismatch { expected: B::ID, actual: erased.bound.kind() })?;
        Ok(Self { tree: erased.tree, args, bound })
    }
}

impl<A, B> Clone for Tooled<A, B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A, B> Copy for Tooled<A, B> {}

impl<A, B> PartialEq for Tooled<A, B> {
    fn eq(&self, other: &Self) -> bool {
        self.tree == other.tree && self.args == other.args && self.bound == other.bound
    }
}

impl<A, B> Eq for Tooled<A, B> {}

impl<A, B> fmt::Debug for Tooled<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tooled")
            .field("tree", &self.tree)
            .field("args", &self.args)
            .field("bound", &self.bound)
            .finish()
    }
}

mod sealed {
    pub trait Sealed {}
}

impl<A, B> sealed::Sealed for Tooled<A, B> {}

/// A program input that makes its program a tool: only [`Tooled<A, B>`],
/// whose arguments `A` are what the model writes and whose bound `B` is what
/// the loop binds.
pub trait ToolArguments: sealed::Sealed {
    /// What the model writes for a call: the arguments the tool reads.
    type Arguments: Storage + Schema + Clone + Cites + Send + 'static;
    /// What the loop binds for a call: the session values the model never
    /// writes.
    type Bound: Storage + Schema + Clone + Cites + Send + 'static;
}

impl<A: Storage + Schema + Clone + Cites + Send + 'static, B: Storage + Schema + Clone + Cites + Send + 'static>
    ToolArguments for Tooled<A, B>
{
    type Arguments = A;
    type Bound = B;
}

fn wire_error(error: &StorageError) -> WireError {
    WireError::Message(alloc::format!("{error}"))
}

impl<A, B> Schema for Tooled<A, B> {
    const SCHEMA: SchemaType = <ErasedTooled as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Tooled"));
    const LABEL_NODE: LabelNode = <ErasedTooled as Schema>::LABEL_NODE;
    const DOC_NODE: DocNode = <ErasedTooled as Schema>::DOC_NODE;
}

impl<A, B> aether_data::CrossesActors for Tooled<A, B> {}
impl<A, B> aether_data::CrossesWire for Tooled<A, B> {}

impl<A: Kind + 'static, B: Kind + 'static> Kind for Tooled<A, B> {
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

impl<A: Kind + 'static, B: Kind + 'static> StorageLeaves for Tooled<A, B> {
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

impl<A: Kind + 'static, B: Kind + 'static> Storage for Tooled<A, B> {
    fn decode_storage(bytes: &[u8]) -> Result<StorageData<Self>, StorageError> {
        decode_derived(bytes, Self::STRICT)
    }

    fn encode_storage(data: &StorageData<Self>) -> Result<Vec<u8>, StorageError> {
        encode_derived(data)
    }
}

impl<A: Kind + 'static, B: Kind + 'static> StorageElement for Tooled<A, B> {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl<A: Kind + 'static, B: Kind + 'static> WireEncode for Tooled<A, B> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.erase().encode(out)
    }
}

impl<'de, A: Kind + 'static, B: Kind + 'static> WireDecode<'de> for Tooled<A, B> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::from_erased(ErasedTooled::decode(cursor)?).map_err(|error| wire_error(&error))
    }
}

impl<A: Kind, B: Kind> Cites for Tooled<A, B> {
    fn cites(&self, sink: &mut Citations) {
        self.tree.cites(sink);
        self.args.cites(sink);
        self.bound.cites(sink);
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ClosureArtifact, EncodedArtifact, ErasedRef, Ref, Tree};
    use aether_data::{Kind, Storage, StorageError};

    use super::{NoBound, Tooled, tooled};
    use crate::ToolSchema;

    #[test]
    fn the_loops_erased_input_is_the_tools_typed_input_and_a_foreign_argument_kind_refuses() {
        // Catches a hand-written codec or citation walk that drifts from the erased form the loop encodes, which
        // would make the tool's input artifact differ from the loop's, a dropped check on the arguments' kind, a
        // dropped bound check, or a cites walk that omits the bound.
        let tree = Ref::<Tree>::of_encoded(&Tree::empty()).expect("tree");
        let args = Ref::<ToolSchema>::of_encoded(&ToolSchema::of::<Tree>()).expect("args");
        let bound = Ref::<NoBound>::of_encoded(&NoBound).expect("bound");
        let erased = EncodedArtifact::new(&tooled(tree, args.erase(), bound.erase())).expect("erased");
        let (kind, payload, citations) = erased.clone().into_parts();
        let payload = ClosureArtifact::new(kind, payload).load(erased.digest()).expect("staged bytes hash");

        let typed = Tooled::<ToolSchema>::decode_storage(&payload).expect("the erased input decodes typed").value;
        assert_eq!((typed.tree(), typed.args(), typed.bound()), (tree, args, bound));
        let reencoded = EncodedArtifact::new(&typed).expect("typed");
        assert_eq!(reencoded.digest(), erased.digest(), "one stored kind, one payload");
        assert_eq!(reencoded.into_parts().2, citations, "the same citations, in order");
        assert_eq!(kind, Tooled::<ToolSchema>::ID);

        let foreign = EncodedArtifact::new(&tooled(tree, ErasedRef::new(Tree::ID, args.digest()), bound.erase()))
            .expect("foreign");
        let digest = foreign.digest();
        let (kind, payload, _) = foreign.into_parts();
        let payload = ClosureArtifact::new(kind, payload).load(digest).expect("staged bytes hash");
        assert!(matches!(
            Tooled::<ToolSchema>::decode_storage(&payload),
            Err(StorageError::TypeMismatch { expected, actual }) if expected == ToolSchema::ID && actual == Tree::ID
        ));

        let foreign_bound = EncodedArtifact::new(&tooled(tree, args.erase(), ErasedRef::new(Tree::ID, bound.digest())))
            .expect("foreign bound");
        let digest = foreign_bound.digest();
        let (kind, payload, _) = foreign_bound.into_parts();
        let payload = ClosureArtifact::new(kind, payload).load(digest).expect("staged bytes hash");
        assert!(matches!(
            Tooled::<ToolSchema>::decode_storage(&payload),
            Err(StorageError::TypeMismatch { expected, actual }) if expected == NoBound::ID && actual == Tree::ID
        ));
    }
}
