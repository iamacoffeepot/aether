//! The kind attribute finds a `Held` field by reading its type's path, so a
//! `Held` hidden behind a type alias is not seen and the kind keeps `Clone`.
//! `Held` has no `Clone` impl, so the derive fails at the aliased field rather
//! than quietly copying a ticket.

use core::marker::PhantomData;

use aether_data::wire::{Decoder, Encoder, Error, WireDecode, WireEncode};
use aether_data::{CastEligible, CrossesActors, KindId, LabelNode, Schema, SchemaType};

/// A move-only ticket shaped like ADR-0243's `Held`: every trait a kind field
/// needs except `Clone`.
#[derive(Debug)]
pub struct Held<R>(u64, PhantomData<R>);

impl<R> Schema for Held<R> {
    const SCHEMA: SchemaType = SchemaType::Ticket { reply: KindId(1) };
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<R> CrossesActors for Held<R> {}

impl<R> CastEligible for Held<R> {
    const ELIGIBLE: bool = false;
}

impl<R> WireEncode for Held<R> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.encode_to(out)
    }

    fn encode_to<E: Encoder + ?Sized>(&self, enc: &mut E) -> Result<(), Error> {
        enc.held(self.0, KindId(1))
    }
}

impl<'de, R> WireDecode<'de> for Held<R> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Self::decode_from(cursor)
    }

    fn decode_from<D: Decoder<'de> + ?Sized>(dec: &mut D) -> Result<Self, Error> {
        let ticket = u64::decode(dec.cursor())?;
        dec.claim_held(ticket, KindId(1))?;
        Ok(Self(ticket, PhantomData))
    }
}

#[derive(Debug)]
pub struct Reply;

type Debt = Held<Reply>;

#[aether_data::kind(name = "test.held.aliased", no_serde)]
pub struct Context {
    pub debt: Debt,
}

fn main() {}
