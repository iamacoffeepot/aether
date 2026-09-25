use alloc::vec;
use alloc::vec::Vec;

use super::{Blob, BlobReader, MAX_READ_BYTES};

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
