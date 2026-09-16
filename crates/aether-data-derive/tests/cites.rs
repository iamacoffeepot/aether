//! Emitted [`aether_data::Cites`] walk over struct fields, containers, and enum variants.

use std::collections::BTreeMap;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError, StorageLeaves};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, KindId, LabelNode, Schema, SchemaType};

const CITE_KIND: KindId = KindId(0x11);

/// Stand-in citation leaf: a `[u8; 32]` with a hand-written [`Cites`] impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CiteKey([u8; 32]);

impl CiteKey {
    fn new(tag: u8) -> Self {
        let mut bytes = [0u8; 32];
        bytes[0] = tag;
        Self(bytes)
    }
}

impl Schema for CiteKey {
    const SCHEMA: SchemaType = <[u8; 32] as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::CiteKey"));
    const LABEL_NODE: LabelNode = <[u8; 32] as Schema>::LABEL_NODE;
}

impl StorageLeaves for CiteKey {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        <[u8; 32] as StorageLeaves>::contribute(&self.0, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self(<[u8; 32] as StorageLeaves>::assemble(carry, depth, source)?))
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        <[u8; 32] as StorageLeaves>::is_absent(carry, depth, source)
    }
}

impl WireEncode for CiteKey {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for CiteKey {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Ok(Self(<[u8; 32] as WireDecode>::decode(cursor)?))
    }
}

impl StorageElement for CiteKey {
    const TAGGED: bool = <[u8; 32] as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.0.contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Ok(Self(<[u8; 32] as StorageElement>::assemble_element(depth, cursor)?))
    }
}

impl Cites for CiteKey {
    fn cites(&self, sink: &mut Citations) {
        sink.push(CITE_KIND, &self.0);
    }
}

fn tags_of(value: &impl Cites) -> Vec<u8> {
    let mut sink = Citations::default();
    value.cites(&mut sink);
    sink.as_slice().iter().map(|citation| citation.bytes[0]).collect()
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.cites.nested")]
struct Nested {
    leaf: CiteKey,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.cites.walk")]
struct Walk {
    plain: CiteKey,
    many: Vec<CiteKey>,
    maybe: Option<CiteKey>,
    mapped: BTreeMap<String, CiteKey>,
    inner: Nested,
}

#[test]
fn emitted_walk_visits_a_citing_leaf_in_each_container_shape() {
    // Catches a derive that walks only named top-level fields and skips
    // Vec, Option, map values, or a nested storage struct.
    let mut mapped = BTreeMap::new();
    mapped.insert("a".into(), CiteKey::new(5));
    mapped.insert("b".into(), CiteKey::new(6));
    let value = Walk {
        plain: CiteKey::new(1),
        many: vec![CiteKey::new(2), CiteKey::new(3)],
        maybe: Some(CiteKey::new(4)),
        mapped,
        inner: Nested { leaf: CiteKey::new(7) },
    };
    assert_eq!(tags_of(&value), vec![1, 2, 3, 4, 5, 6, 7]);
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.cites.shape")]
enum Shape {
    Tuple(CiteKey, u32),
    Named { leaf: CiteKey, n: u32 },
}

#[test]
fn emitted_walk_visits_a_citing_leaf_in_tuple_and_named_enum_variants() {
    // Catches a derive that emits a match arm without binding the variant's
    // fields, which drops citations for exactly the shape a tree entry will use.
    assert_eq!(tags_of(&Shape::Tuple(CiteKey::new(8), 0)), vec![8]);
    assert_eq!(tags_of(&Shape::Named { leaf: CiteKey::new(9), n: 1 }), vec![9]);
}

#[derive(Debug, PartialEq, aether_data::Storage)]
enum NestedCited {
    Off,
    Key { leaf: CiteKey },
}

struct Never;

impl aether_data::Invariant for Never {
    fn reason(&self) -> &'static str {
        "never"
    }
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[storage(validate)]
struct CitedWrap(CiteKey);

impl CitedWrap {
    fn check(inner: &CiteKey) -> Result<(), Never> {
        if inner.0.iter().all(|&byte| byte == 0) {
            Err(Never)
        } else {
            Ok(())
        }
    }
}

#[test]
fn kindless_enum_and_validated_newtype_forward_cites() {
    // Catches a nested enum or validated newtype that implements Cites as a
    // no-op and drops the citation a leaf inside them carries.
    assert_eq!(tags_of(&NestedCited::Key { leaf: CiteKey::new(10) }), vec![10]);
    assert!(tags_of(&NestedCited::Off).is_empty());
    assert_eq!(tags_of(&CitedWrap(CiteKey::new(11))), vec![11]);
}
