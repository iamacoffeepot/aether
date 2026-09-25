use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::array;

use super::{__shared_backing, Blob, BlobBacking, BlobHash, BlobReader, MAX_READ_BYTES, Repr};
use crate::wire::{self, Error};

fn patterned(len: usize) -> Vec<u8> {
    (0..=250).cycle().take(len).collect()
}

/// Catches a missing `MAX_READ_BYTES` clamp, or a cursor that fails to
/// advance past what `read` returned.
#[test]
fn read_clamps_each_call_and_advances_to_the_end() {
    let bytes = patterned(MAX_READ_BYTES + 5);
    let blob = Blob::from(bytes.clone());
    let mut reader = BlobReader::open(&blob);
    let mut buf = vec![0; 2 * MAX_READ_BYTES];

    assert_eq!(reader.read(&mut buf), MAX_READ_BYTES);
    assert_eq!(buf[..MAX_READ_BYTES], bytes[..MAX_READ_BYTES]);

    assert_eq!(reader.read(&mut buf), 5);
    assert_eq!(buf[..5], bytes[MAX_READ_BYTES..]);

    assert_eq!(reader.read(&mut buf), 0);
}

/// Catches an off-by-one window, a `read_range` that moves the cursor, or a
/// read past the end that returns stale bytes.
#[test]
fn seek_and_read_range_address_the_right_window() {
    let bytes = patterned(10);
    let blob = Blob::from(bytes.clone());
    let mut reader = BlobReader::open(&blob);
    let mut buf = [0; 4];

    assert_eq!(reader.read_range(3, &mut buf), 4);
    assert_eq!(buf, bytes[3..7]);
    assert_eq!(reader.read_range(8, &mut buf), 2);
    assert_eq!(buf[..2], bytes[8..]);
    assert_eq!(reader.read_range(10, &mut buf), 0);
    assert_eq!(reader.read_range(11, &mut buf), 0);

    assert_eq!(reader.read(&mut buf), 4);
    assert_eq!(buf, bytes[..4]);

    reader.seek(u64::MAX);
    assert_eq!(reader.read(&mut buf), 0);

    reader.seek(7);
    assert_eq!(reader.read(&mut buf), 3);
    assert_eq!(buf[..3], bytes[7..]);
}

/// A backing that copies at most three bytes per `read_at`, as a guest
/// backing copies at most `MAX_READ_BYTES` per call.
struct ShortReads(Vec<u8>);

impl BlobBacking for ShortReads {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize {
        let Some(rest) = usize::try_from(offset).ok().and_then(|start| self.0.get(start..)) else {
            return 0;
        };
        let copied = rest.len().min(buf.len()).min(3);
        buf[..copied].copy_from_slice(&rest[..copied]);
        copied
    }
}

fn shared(bytes: Vec<u8>) -> Blob {
    Blob(Repr::Shared(Arc::new(ShortReads(bytes))))
}

/// Catches a tag-0 write that assumes one `read_at` fills the buffer, or
/// streams from the wrong offset: a `Shared` value encodes to exactly its
/// bytes through both the typed codec and serde.
#[test]
fn shared_value_streams_its_bytes_into_the_tag_zero_form() {
    let bytes = patterned(10);
    let blob = shared(bytes.clone());

    let mut expected = vec![0, 10, 0, 0, 0];
    expected.extend_from_slice(&bytes);
    assert_eq!(wire::encode_to_vec(&blob).expect("typed encode"), expected);
    assert_eq!(wire::to_vec(&blob).expect("serde encode"), expected);
}

/// Catches a plain decode that fabricates a value from a tag-1 field, or
/// misreads the hash's width: it refuses with the field's own 32-byte hash,
/// and a tag that is neither 0 nor 1 is refused by number.
#[test]
fn plain_decode_refuses_tag_one_with_its_hash_and_unknown_tags() {
    let hash: [u8; 32] = array::from_fn(|i| u8::try_from(i).expect("32 fits a byte"));
    let mut field = vec![1];
    field.extend_from_slice(&hash);

    let err = wire::decode_from_slice::<Blob>(&field).expect_err("a plain decode has no resolver");
    assert_eq!(err, Error::DetachedBlob(BlobHash::from_bytes(hash)));

    let mut nested = vec![1, 0, 0, 0];
    nested.extend_from_slice(&field);
    let err = wire::decode_from_slice::<Vec<Blob>>(&nested).expect_err("nor does one inside a container");
    assert_eq!(err, Error::DetachedBlob(BlobHash::from_bytes(hash)));

    let err = wire::decode_from_slice::<Blob>(&[2, 0, 0, 0, 0]).expect_err("tag 2 names nothing");
    assert_eq!(err, Error::InvalidBlobTag(2));
}

/// Catches an accessor that leaks `Owned` bytes as a backing, or hands back a
/// backing other than the value's own.
#[test]
fn shared_backing_is_only_the_shared_values_own() {
    assert!(__shared_backing(&Blob::from(vec![1])).is_none());

    let backing: Arc<dyn BlobBacking> = Arc::new(ShortReads(vec![1]));
    let blob = Blob(Repr::Shared(Arc::clone(&backing)));
    let found = __shared_backing(&blob).expect("a shared value has a backing");
    assert!(Arc::ptr_eq(found, &backing));
}
