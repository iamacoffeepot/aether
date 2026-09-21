//! [`ActorRef`](crate::ActorRef): proof that an actor reached `Live`.

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

/// Proof that an actor of type `R` reached `Live` at [`id`](Self::id). The
/// constructor is `pub(crate)`: in this change a value comes into being only
/// through wire decode or `Deserialize`, and there is no `From<MailboxId>`.
///
/// ```compile_fail,E0624
/// use aether_data::{ActorRef, MailboxId};
///
/// let _reference = ActorRef::<()>::new(MailboxId::NONE);
/// ```
pub struct ActorRef<R> {
    id: MailboxId,
    _actor: PhantomData<fn() -> R>,
}

impl<R> ActorRef<R> {
    /// Stable type id — FNV-1a of `TYPE_DOMAIN ++ TYPE_NAME`.
    pub const TYPE_ID: u64 = fnv1a_64_prefixed(TYPE_DOMAIN, b"aether.actor_ref");

    /// Canonical name used to compute `TYPE_ID`.
    pub const TYPE_NAME: &'static str = "aether.actor_ref";

    /// Mint a reference for a registered id. Crate-private: the only doors
    /// are wire decode and `Deserialize` in this change.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id, _actor: PhantomData }
    }

    /// The registered id. Free and total; there is no way back from the id
    /// to a reference outside the registry.
    #[must_use]
    pub const fn id(self) -> MailboxId {
        self.id
    }

    /// Forget the actor type, keeping the proof that some actor reached
    /// `Live` at the id.
    #[must_use]
    pub const fn erase(self) -> AnyActorRef {
        AnyActorRef::new(self.id)
    }
}

impl<R> Copy for ActorRef<R> {}

impl<R> Clone for ActorRef<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> PartialEq for ActorRef<R> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<R> Eq for ActorRef<R> {}

impl<R> Hash for ActorRef<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<R> fmt::Debug for ActorRef<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ActorRef").field(&self.id).finish()
    }
}

impl<R> Schema for ActorRef<R> {
    const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
    const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<R> CastEligible for ActorRef<R> {
    const ELIGIBLE: bool = false;
}

impl<R> WireEncode for ActorRef<R> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.id.encode(out)
    }
}

impl<'de, R> WireDecode<'de> for ActorRef<R> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        decode_reference_id(cursor).map(Self::new)
    }
}

impl<R> Serialize for ActorRef<R> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_reference_id(self.id, serializer)
    }
}

impl<'de, R> Deserialize<'de> for ActorRef<R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_reference_id(deserializer).map(Self::new)
    }
}
