//! A typed view of one recorded run of a program: `bloomery.transition` for a known [`Program`].

use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use aether_bloomery_kinds::{ProgramRef, Transition};
use aether_data::storage::{RecordReader, RecordWriter, decode_derived, encode_derived};
use aether_data::{
    Citations, Cites, Kind, KindId, LabelNode, Ref, Schema, SchemaType, Storage, StorageData, StorageError,
    StorageLeaves, storage_kind_id_from_name,
};

use crate::Program;

/// One recorded run of program `P`: the [`Transition`] a driver appended for
/// it, typed by `P` the way [`HeadMoved`](aether_bloomery_kinds::HeadMoved)
/// is typed by its target.
///
/// It is stored as `bloomery.transition`, and decodes only from a transition
/// whose program is named `P::NAME`. A transition of another program is a
/// different typed specialization of the same stored kind, so a rule
/// triggered by `Ran<P>` declines it, while a malformed payload stays a
/// decode error. Its input and result are citations typed by `P`, so the
/// driver delivers both artifacts beside the entry.
pub struct Ran<P> {
    transition: Transition,
    _program: PhantomData<fn() -> P>,
}

impl<P: Program> Ran<P> {
    /// The recorded program: its bundle digest and name.
    #[must_use]
    pub const fn program(&self) -> &ProgramRef {
        &self.transition.program
    }

    /// The run's input artifact, a `P::Input`.
    #[must_use]
    pub fn input(&self) -> Ref<P::Input> {
        Ref::from_digest(self.transition.input)
    }

    /// The run's result artifact, a `P::Result`.
    #[must_use]
    pub fn result(&self) -> Ref<P::Result> {
        Ref::from_digest(self.transition.result)
    }

    /// Type `transition` as a run of `P`, or refuse it as another program's.
    fn typed(transition: Transition) -> Result<Self, StorageError> {
        let name = transition.program.name().as_str();
        if name != P::NAME {
            return Err(StorageError::TypeMismatch {
                expected: storage_kind_id_from_name(P::NAME),
                actual: storage_kind_id_from_name(name),
            });
        }
        Ok(Self { transition, _program: PhantomData })
    }
}

impl<P> Clone for Ran<P> {
    fn clone(&self) -> Self {
        Self { transition: self.transition.clone(), _program: PhantomData }
    }
}

impl<P> PartialEq for Ran<P> {
    fn eq(&self, other: &Self) -> bool {
        self.transition == other.transition
    }
}

impl<P> Eq for Ran<P> {}

impl<P> fmt::Debug for Ran<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ran").field(&self.transition).finish()
    }
}

impl<P> Schema for Ran<P> {
    const SCHEMA: SchemaType = <Transition as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Ran"));
    const LABEL_NODE: LabelNode = <Transition as Schema>::LABEL_NODE;
}

impl<P> aether_data::CrossesActors for Ran<P> {}
impl<P> aether_data::CrossesWire for Ran<P> {}

impl<P: Program> Kind for Ran<P> {
    const NAME: &'static str = <Transition as Kind>::NAME;
    const ID: KindId = <Transition as Kind>::ID;

    fn encode_into_bytes(&self) -> Vec<u8> {
        self.transition.encode_into_bytes()
    }
}

impl<P: Program> StorageLeaves for Ran<P> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.transition.contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Self::typed(Transition::assemble(carry, depth, source)?)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        Transition::is_absent(carry, depth, source)
    }
}

impl<P: Program> Storage for Ran<P> {
    fn decode_storage(bytes: &[u8]) -> Result<StorageData<Self>, StorageError> {
        decode_derived(bytes, Self::STRICT)
    }

    fn encode_storage(data: &StorageData<Self>) -> Result<Vec<u8>, StorageError> {
        encode_derived(data)
    }
}

impl<P: Program> Cites for Ran<P> {
    fn cites(&self, sink: &mut Citations) {
        self.input().cites(sink);
        self.result().cites(sink);
    }
}
