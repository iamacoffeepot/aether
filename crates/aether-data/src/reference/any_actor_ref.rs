//! [`AnyActorRef`](crate::AnyActorRef): proof that some actor reached `Live`.

use alloc::vec::Vec;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::id_codec::{decode_reference_id, deserialize_reference_id, serialize_reference_id};
use crate::hash::{TYPE_DOMAIN, fnv1a_64_prefixed};
use crate::schema::{LabelNode, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode};
use crate::{CastEligible, MailboxId, Schema};

/// Proof that some actor reached `Live` at [`id`](Self::id), with the actor
/// type erased. This is what [`ActorRef::erase`](crate::ActorRef::erase) and
/// [`Recipient::erase`](crate::Recipient::erase) produce. It carries no
/// phantom parameter, so the value traits are derived rather than
/// hand-written; like its siblings the constructor is `pub(crate)`, and the
/// only doors in this change are wire decode and `Deserialize`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct AnyActorRef {
    id: MailboxId,
}

impl AnyActorRef {
    /// Stable type id — FNV-1a of `TYPE_DOMAIN ++ TYPE_NAME`.
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.any_actor_ref");

    /// Canonical name used to compute `TYPE_ID`.
    pub const TYPE_NAME: &'static str = "aether.any_actor_ref";

    /// Mint an erased reference for a registered id. Crate-private: the
    /// only doors are wire decode and `Deserialize` in this change.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id }
    }

    /// The registered id. Free and total.
    #[must_use]
    pub const fn id(self) -> MailboxId {
        self.id
    }
}

impl Schema for AnyActorRef {
    const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
    const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl CastEligible for AnyActorRef {
    const ELIGIBLE: bool = false;
}

impl WireEncode for AnyActorRef {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.id.encode(out)
    }
}

impl<'de> WireDecode<'de> for AnyActorRef {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        decode_reference_id(cursor).map(Self::new)
    }
}

impl Serialize for AnyActorRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_reference_id(self.id, serializer)
    }
}

impl<'de> Deserialize<'de> for AnyActorRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_reference_id(deserializer).map(Self::new)
    }
}
