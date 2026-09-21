//! [`Tombstone`](crate::Tombstone): proof that an actor died.

use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::id_codec::{decode_reference_id, deserialize_reference_id, serialize_reference_id};
use crate::hash::{TYPE_DOMAIN, fnv1a_64_prefixed};
use crate::schema::{LabelNode, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode};
use crate::{CastEligible, MailboxId, Schema};

/// Proof that the actor of type `R` at [`id`](Self::id) is dead: the
/// terminal state under ADR-0230's monotone order, exchanged for a reference
/// on its `MonitorNotice`. Like [`ActorRef`](crate::ActorRef) the
/// constructor is `pub(crate)`, and the only doors in this change are wire
/// decode and `Deserialize`.
pub struct Tombstone<R> {
    id: MailboxId,
    _actor: PhantomData<fn() -> R>,
}

impl<R> Tombstone<R> {
    /// Stable type id — FNV-1a of `TYPE_DOMAIN ++ TYPE_NAME`.
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.tombstone");

    /// Canonical name used to compute `TYPE_ID`.
    pub const TYPE_NAME: &'static str = "aether.tombstone";

    /// Mint a tombstone for a dead id. Crate-private: the only doors are
    /// wire decode and `Deserialize` in this change.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id, _actor: PhantomData }
    }

    /// The dead id. Free and total.
    #[must_use]
    pub const fn id(self) -> MailboxId {
        self.id
    }
}

impl<R> Copy for Tombstone<R> {}

impl<R> Clone for Tombstone<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> PartialEq for Tombstone<R> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<R> Eq for Tombstone<R> {}

impl<R> Hash for Tombstone<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<R> fmt::Debug for Tombstone<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Tombstone").field(&self.id).finish()
    }
}

impl<R> Schema for Tombstone<R> {
    const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
    const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<R> CastEligible for Tombstone<R> {
    const ELIGIBLE: bool = false;
}

impl<R> WireEncode for Tombstone<R> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.id.encode(out)
    }
}

impl<'de, R> WireDecode<'de> for Tombstone<R> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        decode_reference_id(cursor).map(Self::new)
    }
}

impl<R> Serialize for Tombstone<R> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_reference_id(self.id, serializer)
    }
}

impl<'de, R> Deserialize<'de> for Tombstone<R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_reference_id(deserializer).map(Self::new)
    }
}
