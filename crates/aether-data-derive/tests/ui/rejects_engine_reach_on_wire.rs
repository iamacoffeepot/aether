//! A leaf of engine reach crosses actors and not the wire (ADR-0242), so a
//! kind holding one sends as `ActorMail` and fails a `WireMail` bound.

use aether_data::wire::{Error, WireDecode, WireEncode};
use aether_data::{CrossesActors, LabelNode, Schema, SchemaType};

/// A test-local leaf of engine reach.
#[derive(Clone, Debug)]
struct EngineLeaf(u32);

impl Schema for EngineLeaf {
    const SCHEMA: SchemaType = <u32 as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl CrossesActors for EngineLeaf {}

impl WireEncode for EngineLeaf {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for EngineLeaf {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        u32::decode(cursor).map(Self)
    }
}

#[aether_data::kind(name = "test.reach.engine", no_serde)]
struct Notice {
    count: u32,
    leaves: Vec<EngineLeaf>,
}

fn send<K: aether_data::ActorMail>(_: &K) {}

fn call<K: aether_data::WireMail>(_: &K) {}

fn main() {
    let notice = Notice { count: 0, leaves: Vec::new() };
    send(&notice);
    call(&notice);
}
