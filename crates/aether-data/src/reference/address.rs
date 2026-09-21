//! [`Address`](crate::Address): a well-formed description of where an actor lives.

use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use serde::{Deserialize, Serialize};

use super::actor_ref::ActorRef;
use super::load_name::LoadName;
use crate::schema::{LabelNode, NamedField, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode};
use crate::{CastEligible, MailboxId, Schema};

/// A well-formed description of where an actor of type `R` lives: an
/// optional anchor plus an optional key. An address claims nothing about
/// existence — only resolution against the registry turns one into a
/// reference — so unlike the proven types its constructors are public.
///
/// The value traits are hand-written and the serde bound is emptied so that
/// none of them lands a bound on the phantom `R`.
#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Address<R> {
    anchor: Option<MailboxId>,
    key: Option<LoadName>,
    _actor: PhantomData<fn() -> R>,
}

impl<R> Address<R> {
    /// An address with no anchor: the key alone, resolved against the
    /// ambient scope.
    #[must_use]
    pub const fn scoped(key: Option<LoadName>) -> Self {
        Self { anchor: None, key, _actor: PhantomData }
    }

    /// An address beneath a proven `parent`: the parent's registered id
    /// anchors the key, so the description cannot drift from a live actor.
    #[must_use]
    pub fn beneath<P>(parent: ActorRef<P>, key: Option<LoadName>) -> Self {
        Self { anchor: Some(parent.id()), key, _actor: PhantomData }
    }

    /// The anchoring id, when the address sits beneath a proven parent.
    #[must_use]
    pub fn anchor(&self) -> Option<MailboxId> {
        self.anchor
    }

    /// The key within the anchor's scope, when the address carries one.
    #[must_use]
    pub fn key(&self) -> Option<&LoadName> {
        self.key.as_ref()
    }
}

impl<R> Clone for Address<R> {
    fn clone(&self) -> Self {
        Self { anchor: self.anchor, key: self.key.clone(), _actor: PhantomData }
    }
}

impl<R> PartialEq for Address<R> {
    fn eq(&self, other: &Self) -> bool {
        self.anchor == other.anchor && self.key == other.key
    }
}

impl<R> Eq for Address<R> {}

impl<R> Hash for Address<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.anchor.hash(state);
        self.key.hash(state);
    }
}

impl<R> fmt::Debug for Address<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Address").field("anchor", &self.anchor).field("key", &self.key).finish()
    }
}

impl<R> Schema for Address<R> {
    const SCHEMA: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[
            NamedField { name: Cow::Borrowed("anchor"), ty: <Option<MailboxId> as Schema>::SCHEMA },
            NamedField { name: Cow::Borrowed("key"), ty: <Option<LoadName> as Schema>::SCHEMA },
        ]),
        repr_c: false,
    };
    const LABEL: Option<&'static str> = Some("aether.address");
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<R> CastEligible for Address<R> {
    const ELIGIBLE: bool = false;
}

impl<R> WireEncode for Address<R> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.anchor.encode(out)?;
        self.key.encode(out)
    }
}

impl<'de, R> WireDecode<'de> for Address<R> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Ok(Self {
            anchor: Option::<MailboxId>::decode(cursor)?,
            key: Option::<LoadName>::decode(cursor)?,
            _actor: PhantomData,
        })
    }
}
