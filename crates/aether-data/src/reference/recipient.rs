//! [`Recipient`](crate::Recipient): proof that a `K`-handling actor reached `Live`.

use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::any_actor_ref::AnyActorRef;
use super::id_codec::{decode_reference_id, deserialize_reference_id, serialize_reference_id};
use crate::hash::{TYPE_DOMAIN, fnv1a_64_prefixed};
use crate::schema::{LabelNode, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode};
use crate::{CastEligible, MailboxId, Schema};

/// Proof that an actor handling kind `K` reached `Live` at
/// [`id`](Self::id). Carries `K` as an unbounded phantom: the
/// `HandlesKind` bound lives on the operations above `aether-data`. Like
/// [`ActorRef`](crate::ActorRef) the constructor is `pub(crate)`, and the
/// only doors in this change are wire decode and `Deserialize`.
pub struct Recipient<K> {
    id: MailboxId,
    _kind: PhantomData<fn(K)>,
}

impl<K> Recipient<K> {
    /// Stable type id — FNV-1a of `TYPE_DOMAIN ++ TYPE_NAME`.
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.recipient");

    /// Canonical name used to compute `TYPE_ID`.
    pub const TYPE_NAME: &'static str = "aether.recipient";

    /// Mint a recipient for a registered id. Crate-private: the only doors
    /// are wire decode and `Deserialize` in this change.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id, _kind: PhantomData }
    }

    /// The registered id. Free and total.
    #[must_use]
    pub const fn id(self) -> MailboxId {
        self.id
    }

    /// Forget the handled kind, keeping the proof that some actor reached
    /// `Live` at the id.
    #[must_use]
    pub const fn erase(self) -> AnyActorRef {
        AnyActorRef::new(self.id)
    }
}

impl<K> Copy for Recipient<K> {}

impl<K> Clone for Recipient<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> PartialEq for Recipient<K> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<K> Eq for Recipient<K> {}

impl<K> Hash for Recipient<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<K> fmt::Debug for Recipient<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Recipient").field(&self.id).finish()
    }
}

impl<K> Schema for Recipient<K> {
    const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
    const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<K> CastEligible for Recipient<K> {
    const ELIGIBLE: bool = false;
}

impl<K> WireEncode for Recipient<K> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.id.encode(out)
    }
}

impl<'de, K> WireDecode<'de> for Recipient<K> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        decode_reference_id(cursor).map(Self::new)
    }
}

impl<K> Serialize for Recipient<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_reference_id(self.id, serializer)
    }
}

impl<'de, K> Deserialize<'de> for Recipient<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_reference_id(deserializer).map(Self::new)
    }
}
