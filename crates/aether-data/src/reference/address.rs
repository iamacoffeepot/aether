//! [`Address`]: a well-formed description of which actor is meant.

use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use serde::{Deserialize, Serialize};

use super::load_name::LoadName;
use crate::schema::{EnumVariant, LabelNode, NamedField, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode};
use crate::{CastEligible, MailboxId, Schema};

/// How an [`Address`] denotes its actor. Every form names a position; none
/// claims that anything is registered there.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum AddressForm {
    /// Relative to the scope the actor type's resolver selects from the
    /// caller, with `key` as the discriminator when the type is keyed.
    Scoped { key: Option<LoadName> },
    /// Directly beneath the actor at `parent`.
    Beneath { parent: MailboxId, key: Option<LoadName> },
    /// Exactly the position `id`. This is how a held reference is handed to
    /// a peer.
    Exact { id: MailboxId },
}

/// A description of which actor of type `R` is meant. An address claims
/// nothing about existence, so its constructors are public and it is the one
/// reference form with a codec: it can ride in mail, sit in a config, and be
/// persisted, and only resolving it against the registry yields a reference
/// (ADR-0230).
///
/// The value traits are hand-written and the serde bound is emptied so that
/// none of them lands a bound on the phantom `R`.
#[derive(Serialize, Deserialize)]
#[serde(transparent, bound = "")]
pub struct Address<R> {
    form: AddressForm,
    #[serde(skip)]
    _actor: PhantomData<fn() -> R>,
}

impl<R> Address<R> {
    /// An address resolved relative to the caller's scope.
    #[must_use]
    pub const fn scoped(key: Option<LoadName>) -> Self {
        Self { form: AddressForm::Scoped { key }, _actor: PhantomData }
    }

    /// An address directly beneath the actor at `parent`. Takes a position:
    /// the typed constructor that demands a proven parent lives with the
    /// proven types.
    #[must_use]
    pub const fn beneath(parent: MailboxId, key: Option<LoadName>) -> Self {
        Self { form: AddressForm::Beneath { parent, key }, _actor: PhantomData }
    }

    /// The address of exactly the position `id`.
    #[must_use]
    pub const fn exact(id: MailboxId) -> Self {
        Self { form: AddressForm::Exact { id }, _actor: PhantomData }
    }

    /// How this address denotes its actor.
    #[must_use]
    pub const fn form(&self) -> &AddressForm {
        &self.form
    }
}

impl<R> Clone for Address<R> {
    fn clone(&self) -> Self {
        Self { form: self.form.clone(), _actor: PhantomData }
    }
}

impl<R> PartialEq for Address<R> {
    fn eq(&self, other: &Self) -> bool {
        self.form == other.form
    }
}

impl<R> Eq for Address<R> {}

impl<R> Hash for Address<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.form.hash(state);
    }
}

impl<R> fmt::Debug for Address<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Address").field(&self.form).finish()
    }
}

const SCOPED: u32 = 0;
const BENEATH: u32 = 1;
const EXACT: u32 = 2;

impl Schema for AddressForm {
    const SCHEMA: SchemaType = SchemaType::Enum {
        variants: Cow::Borrowed(&[
            EnumVariant::Struct {
                name: Cow::Borrowed("Scoped"),
                discriminant: SCOPED,
                fields: Cow::Borrowed(&[NamedField {
                    name: Cow::Borrowed("key"),
                    ty: <Option<LoadName> as Schema>::SCHEMA,
                }]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Beneath"),
                discriminant: BENEATH,
                fields: Cow::Borrowed(&[
                    NamedField { name: Cow::Borrowed("parent"), ty: SchemaType::TypeId(MailboxId::TYPE_ID) },
                    NamedField { name: Cow::Borrowed("key"), ty: <Option<LoadName> as Schema>::SCHEMA },
                ]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Exact"),
                discriminant: EXACT,
                fields: Cow::Borrowed(&[NamedField {
                    name: Cow::Borrowed("id"),
                    ty: SchemaType::TypeId(MailboxId::TYPE_ID),
                }]),
            },
        ]),
    };
    const LABEL: Option<&'static str> = Some("aether.address_form");
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<R> Schema for Address<R> {
    const SCHEMA: SchemaType = AddressForm::SCHEMA;
    const LABEL: Option<&'static str> = Some("aether.address");
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl CastEligible for AddressForm {
    const ELIGIBLE: bool = false;
}

impl<R> CastEligible for Address<R> {
    const ELIGIBLE: bool = false;
}

impl WireEncode for AddressForm {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        match self {
            Self::Scoped { key } => {
                SCOPED.encode(out)?;
                key.encode(out)
            }
            Self::Beneath { parent, key } => {
                BENEATH.encode(out)?;
                parent.encode(out)?;
                key.encode(out)
            }
            Self::Exact { id } => {
                EXACT.encode(out)?;
                id.encode(out)
            }
        }
    }
}

impl<'de> WireDecode<'de> for AddressForm {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        match u32::decode(cursor)? {
            SCOPED => Ok(Self::Scoped { key: Option::<LoadName>::decode(cursor)? }),
            BENEATH => {
                Ok(Self::Beneath { parent: MailboxId::decode(cursor)?, key: Option::<LoadName>::decode(cursor)? })
            }
            EXACT => Ok(Self::Exact { id: MailboxId::decode(cursor)? }),
            other => Err(WireError::InvalidAddressForm(other)),
        }
    }
}

impl<R> WireEncode for Address<R> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.form.encode(out)
    }
}

impl<'de, R> WireDecode<'de> for Address<R> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        AddressForm::decode(cursor).map(|form| Self { form, _actor: PhantomData })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_an_unknown_form() {
        let bytes = 3u32.to_le_bytes();
        let mut cursor: &[u8] = &bytes;
        assert_eq!(AddressForm::decode(&mut cursor), Err(WireError::InvalidAddressForm(3)));
    }
}
