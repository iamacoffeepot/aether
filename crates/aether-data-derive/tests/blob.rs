//! `Blob` fields through the derived codec (ADR-0238 decision 3): the tag-0
//! layout on the plain path, serde agreement, and the encoder and resolver
//! hooks reaching every field wherever it nests.

use std::collections::BTreeMap;

use aether_data::canonical::kind_id_from_shape;
use aether_data::wire::{self, BlobResolver, Encoder};
use aether_data::{Blob, BlobHash, BlobReader, Kind, KindShape, SchemaShape};

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
struct Inner {
    blob: Blob,
}

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
enum Choice {
    Empty,
    Held(Blob),
}

#[aether_data::kind(name = "test.blob.carrier")]
struct Carrier {
    one: Blob,
    maybe: Option<Blob>,
    absent: Option<Blob>,
    many: Vec<Blob>,
    inner: Inner,
    choice: Choice,
    raw: Vec<u8>,
}

/// Every byte of `blob`, looped through its reader.
fn read_all(blob: &Blob) -> Vec<u8> {
    let mut reader = BlobReader::open(blob);
    let mut out = Vec::new();
    let mut buf = [0; 7];
    loop {
        let copied = reader.read(&mut buf);
        if copied == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..copied]);
    }
}

fn carrier() -> Carrier {
    Carrier {
        one: Blob::from(vec![1, 2, 3]),
        maybe: Some(Blob::from(vec![4])),
        absent: None,
        many: vec![Blob::from(vec![5, 6]), Blob::from(Vec::new())],
        inner: Inner { blob: Blob::from(vec![7]) },
        choice: Choice::Held(Blob::from(vec![8, 9])),
        raw: vec![10],
    }
}

/// Each `Blob` of `carrier()`, in field order.
fn blob_bytes(value: &Carrier) -> Vec<Vec<u8>> {
    let Choice::Held(held) = &value.choice else {
        panic!("carrier() holds a blob in `choice`")
    };
    let mut blobs = vec![&value.one];
    blobs.extend(value.maybe.iter());
    blobs.extend(value.absent.iter());
    blobs.extend(value.many.iter());
    blobs.extend([&value.inner.blob, held]);
    blobs.into_iter().map(read_all).collect()
}

/// Catches a container that drops the hook (writing a `Blob` without its tag,
/// or not at all), or a wrong length prefix: the plain encode must be the
/// tag-0 layout, and the plain decode must give back the same bytes.
#[test]
fn plain_encode_writes_tag_zero_everywhere_and_decodes_back() {
    let value = carrier();

    // Tripwire: the tag-0 binary form of a `Blob` field is `[0][u32 LE len][bytes]`,
    // inside every container, and it is what files and the wire carry.
    let expected: Vec<u8> = [
        &[0, 3, 0, 0, 0, 1, 2, 3][..],
        &[1, 0, 1, 0, 0, 0, 4],
        &[0],
        &[2, 0, 0, 0, 0, 2, 0, 0, 0, 5, 6, 0, 0, 0, 0, 0],
        &[0, 1, 0, 0, 0, 7],
        &[1, 0, 0, 0, 0, 2, 0, 0, 0, 8, 9],
        &[1, 0, 0, 0, 10],
    ]
    .concat();
    assert_eq!(value.encode_into_bytes(), expected);

    let back = Carrier::decode_from_bytes(&expected).expect("decode the tag-0 layout");
    assert_eq!(blob_bytes(&back), blob_bytes(&value));
    assert!(back.absent.is_none());
    assert_eq!(back.raw, value.raw);
}

/// Catches serde/wire divergence: `save_state_kind` writes a kind through
/// serde `wire::to_vec`, and its bytes must be the same tag-0 form the typed
/// codec writes and reads.
#[test]
fn serde_wire_bytes_equal_the_typed_encode() {
    let value = carrier();
    let typed = value.encode_into_bytes();

    assert_eq!(wire::to_vec(&value).expect("serde encode"), typed);

    let back: Carrier = wire::from_bytes(&typed).expect("serde decode of the typed bytes");
    assert_eq!(blob_bytes(&back), blob_bytes(&value));
}

/// Writes each `Blob` as tag 1 with a hash distinct per call, and records the
/// bytes it was handed.
#[derive(Default)]
struct Recording {
    out: Vec<u8>,
    seen: Vec<Vec<u8>>,
}

impl Encoder for Recording {
    fn out(&mut self) -> &mut Vec<u8> {
        &mut self.out
    }

    fn blob(&mut self, value: &Blob) -> Result<(), wire::Error> {
        let index = u8::try_from(self.seen.len()).expect("fewer than 256 blobs");
        self.seen.push(read_all(value));
        self.out.push(1);
        self.out.extend_from_slice(&[index; 32]);
        Ok(())
    }
}

/// Resolves the hashes `Recording` wrote, and records each one it was asked for.
struct Table {
    entries: BTreeMap<[u8; 32], Vec<u8>>,
    asked: Vec<[u8; 32]>,
}

impl BlobResolver for Table {
    fn resolve(&mut self, hash: BlobHash) -> Result<Blob, wire::Error> {
        self.asked.push(*hash.as_bytes());
        self.entries.get(hash.as_bytes()).map(|bytes| Blob::from(bytes.clone())).ok_or(wire::Error::DetachedBlob(hash))
    }
}

/// Catches a double or missing hook call, or a resolver asked for the wrong
/// hash: an overriding encoder sees each `Blob` field once, in field order,
/// wherever it nests, and `decode_with` resolves exactly the hashes it wrote.
#[test]
fn hooks_see_each_blob_field_once_and_resolve_its_hash() {
    let value = carrier();
    let expected = blob_bytes(&value);

    let mut recording = Recording::default();
    value.encode_with(&mut recording).expect("encode through the recording hook");
    assert_eq!(recording.seen, expected);

    let hashes: Vec<[u8; 32]> =
        (0..expected.len()).map(|index| [u8::try_from(index).expect("few blobs"); 32]).collect();
    let mut table =
        Table { entries: hashes.iter().copied().zip(expected.iter().cloned()).collect(), asked: Vec::new() };
    let back = Carrier::decode_with(&recording.out, &mut table).expect("decode through the resolver");
    assert_eq!(table.asked, hashes);
    assert_eq!(blob_bytes(&back), expected);

    assert!(Carrier::decode_from_bytes(&recording.out).is_none(), "a plain decode refuses tag 1");
}

#[aether_data::kind(name = "test.blob.twin")]
struct BlobTwin {
    payload: Blob,
}

#[aether_data::kind(name = "test.blob.twin")]
struct BytesTwin {
    payload: Vec<u8>,
}

/// Catches selector reuse, and the compile-time and runtime canonical
/// encoders disagreeing on the `Blob` selector: the derive's const id must
/// equal the id the substrate re-derives from a wasm component's decoded
/// shape, and must differ from a `Vec<u8>` twin's.
#[test]
fn blob_field_hashes_apart_from_bytes_and_matches_the_runtime_id() {
    let shape = KindShape {
        name: BlobTwin::NAME.into(),
        schema: SchemaShape::Struct { fields: vec![SchemaShape::Blob], repr_c: false },
    };

    // Tripwire: `Blob` is canonical selector 12 in both the const-fn and the
    // runtime encoder, and never selector 4 (`Bytes`).
    assert_eq!(BlobTwin::ID.0, kind_id_from_shape(&shape));
    assert_ne!(BlobTwin::ID, BytesTwin::ID);
}
