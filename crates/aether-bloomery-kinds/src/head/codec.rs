//! Flat event codec and standalone symbol identity codec.
//!
//! Public [`HeadMoved`] / [`RecordedHeadMove`] keep a nested Rust shape.
//! Their storage, wire, and container paths flatten to the historical
//! `target_kind` / `symbol` / `to` leaves so old bytes stay identical.
//! Standalone symbols encode `kind` plus `name`.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::storage::{
    RecordReader, RecordWriter, StorageElement, assemble_tagged_element, assert_unique_storage_leaves,
    contribute_tagged_element, decode_derived, encode_derived,
};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{
    Citations, Cites, Kind, KindId, LabelNode, Schema, SchemaType, Storage, StorageData, StorageError, StorageLeaves,
};

use crate::{Digest, Ref};

use super::symbol::symbol_storage_err;
use super::{HeadMoved, RecordedHeadMove, RecordedSymbol, Symbol};

#[derive(Clone, Debug, aether_data::Storage)]
struct FlatHeadMoved {
    target_kind: KindId,
    symbol: String,
    to: Digest,
}

#[derive(Clone, Debug, aether_data::Storage)]
struct FlatSymbol {
    kind: KindId,
    name: String,
}

static EVENT_SCHEMA: SchemaType = <FlatHeadMoved as Schema>::SCHEMA;
static SYMBOL_SCHEMA: SchemaType = <FlatSymbol as Schema>::SCHEMA;
const _: () = assert_unique_storage_leaves(&EVENT_SCHEMA, &[]);
const _: () = assert_unique_storage_leaves(&SYMBOL_SCHEMA, &[]);

const HEAD_MOVED_NAME: &str = "bloomery.head_moved";

fn storage_encode_panic(name: &str) -> ! {
    panic!(
        "aether-data: Kind::encode_into_bytes called on storage kind `{name}`. \
         Storage values do not have a positional mail codec; they reach mail \
         only through handle indirection."
    )
}

fn symbol_wire_err(error: StorageError) -> WireError {
    match error {
        StorageError::Invariant { reason, .. } => WireError::Message(String::from(reason)),
        other => WireError::Message(alloc::format!("{other}")),
    }
}

fn recorded_symbol_from_flat(flat: FlatSymbol) -> Result<RecordedSymbol, StorageError> {
    RecordedSymbol::new(flat.kind, flat.name).map_err(symbol_storage_err)
}

fn typed_symbol_from_flat<K: Kind>(flat: FlatSymbol) -> Result<Symbol<K>, StorageError> {
    Symbol::from_recorded(recorded_symbol_from_flat(flat)?)
}

fn flat_from_recorded_symbol(symbol: &RecordedSymbol) -> FlatSymbol {
    FlatSymbol { kind: symbol.kind(), name: String::from(symbol.as_str()) }
}

fn recorded_move_from_flat(flat: FlatHeadMoved) -> Result<RecordedHeadMove, StorageError> {
    Ok(RecordedHeadMove::new(RecordedSymbol::new(flat.target_kind, flat.symbol).map_err(symbol_storage_err)?, flat.to))
}

fn typed_move_from_flat<K: Kind>(flat: FlatHeadMoved) -> Result<HeadMoved<K>, StorageError> {
    let recorded = recorded_move_from_flat(flat)?;
    Ok(HeadMoved::from_parts(Symbol::from_recorded(recorded.symbol().clone())?, Ref::from_digest(recorded.to())))
}

fn flat_from_recorded_move(event: &RecordedHeadMove) -> FlatHeadMoved {
    FlatHeadMoved { target_kind: event.symbol().kind(), symbol: String::from(event.symbol().as_str()), to: event.to() }
}

fn flat_from_typed_move<K: Kind>(event: &HeadMoved<K>) -> FlatHeadMoved {
    FlatHeadMoved { target_kind: K::ID, symbol: String::from(event.symbol().as_str()), to: event.to().digest() }
}

impl Schema for RecordedSymbol {
    const SCHEMA: SchemaType = <FlatSymbol as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::RecordedSymbol"));
    const LABEL_NODE: LabelNode = <FlatSymbol as Schema>::LABEL_NODE;
}

impl<K> Schema for Symbol<K> {
    const SCHEMA: SchemaType = <FlatSymbol as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Symbol"));
    const LABEL_NODE: LabelNode = <FlatSymbol as Schema>::LABEL_NODE;
}

impl StorageLeaves for RecordedSymbol {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        flat_from_recorded_symbol(self).contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        recorded_symbol_from_flat(FlatSymbol::assemble(carry, depth, source)?)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        FlatSymbol::is_absent(carry, depth, source)
    }
}

impl<K: Kind + 'static> StorageLeaves for Symbol<K> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        RecordedSymbol::from(self).contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        typed_symbol_from_flat(FlatSymbol::assemble(carry, depth, source)?)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        FlatSymbol::is_absent(carry, depth, source)
    }
}

impl StorageElement for RecordedSymbol {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl<K: Kind + 'static> StorageElement for Symbol<K> {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl WireEncode for RecordedSymbol {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        flat_from_recorded_symbol(self).encode(out)
    }
}

impl<'de> WireDecode<'de> for RecordedSymbol {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        recorded_symbol_from_flat(FlatSymbol::decode(cursor)?).map_err(symbol_wire_err)
    }
}

impl<K: Kind + 'static> WireEncode for Symbol<K> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        RecordedSymbol::from(self).encode(out)
    }
}

impl<'de, K: Kind + 'static> WireDecode<'de> for Symbol<K> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        typed_symbol_from_flat(FlatSymbol::decode(cursor)?).map_err(symbol_wire_err)
    }
}

impl Cites for RecordedSymbol {
    fn cites(&self, _sink: &mut Citations) {}
}

impl<K> Cites for Symbol<K> {
    fn cites(&self, _sink: &mut Citations) {}
}

impl Schema for RecordedHeadMove {
    const SCHEMA: SchemaType = <FlatHeadMoved as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::RecordedHeadMove"));
    const LABEL_NODE: LabelNode = <FlatHeadMoved as Schema>::LABEL_NODE;
}

impl<K> Schema for HeadMoved<K> {
    const SCHEMA: SchemaType = <FlatHeadMoved as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::HeadMoved"));
    const LABEL_NODE: LabelNode = <FlatHeadMoved as Schema>::LABEL_NODE;
}

impl Kind for RecordedHeadMove {
    const NAME: &'static str = HEAD_MOVED_NAME;
    const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);

    fn encode_into_bytes(&self) -> Vec<u8> {
        storage_encode_panic(Self::NAME)
    }
}

impl<K: Kind + 'static> Kind for HeadMoved<K> {
    const NAME: &'static str = HEAD_MOVED_NAME;
    const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);

    fn encode_into_bytes(&self) -> Vec<u8> {
        storage_encode_panic(Self::NAME)
    }
}

impl StorageLeaves for RecordedHeadMove {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        flat_from_recorded_move(self).contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        recorded_move_from_flat(FlatHeadMoved::assemble(carry, depth, source)?)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        FlatHeadMoved::is_absent(carry, depth, source)
    }
}

impl<K: Kind + 'static> StorageLeaves for HeadMoved<K> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        flat_from_typed_move(self).contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        typed_move_from_flat(FlatHeadMoved::assemble(carry, depth, source)?)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        FlatHeadMoved::is_absent(carry, depth, source)
    }
}

impl Storage for RecordedHeadMove {
    fn decode_storage(bytes: &[u8]) -> Result<StorageData<Self>, StorageError> {
        decode_derived(bytes, Self::STRICT)
    }

    fn encode_storage(data: &StorageData<Self>) -> Result<Vec<u8>, StorageError> {
        encode_derived(data)
    }
}

impl<K: Kind + 'static> Storage for HeadMoved<K> {
    fn decode_storage(bytes: &[u8]) -> Result<StorageData<Self>, StorageError> {
        decode_derived(bytes, Self::STRICT)
    }

    fn encode_storage(data: &StorageData<Self>) -> Result<Vec<u8>, StorageError> {
        encode_derived(data)
    }
}

impl StorageElement for RecordedHeadMove {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl<K: Kind + 'static> StorageElement for HeadMoved<K> {
    const TAGGED: bool = true;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_tagged_element(self, depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        assemble_tagged_element(depth, cursor)
    }
}

impl WireEncode for RecordedHeadMove {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        flat_from_recorded_move(self).encode(out)
    }
}

impl<'de> WireDecode<'de> for RecordedHeadMove {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        recorded_move_from_flat(FlatHeadMoved::decode(cursor)?).map_err(symbol_wire_err)
    }
}

impl<K: Kind + 'static> WireEncode for HeadMoved<K> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        flat_from_typed_move(self).encode(out)
    }
}

impl<'de, K: Kind + 'static> WireDecode<'de> for HeadMoved<K> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        typed_move_from_flat(FlatHeadMoved::decode(cursor)?).map_err(symbol_wire_err)
    }
}

impl Cites for RecordedHeadMove {
    fn cites(&self, _sink: &mut Citations) {}
}

impl<K> Cites for HeadMoved<K> {
    fn cites(&self, _sink: &mut Citations) {}
}
