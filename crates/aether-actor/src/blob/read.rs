//! Reading a whole blob into guest memory, for the actor that wants the
//! payload in hand.

use aether_data::{Blob, BlobReader};
use alloc::vec;
use alloc::vec::Vec;

/// Every byte of `blob`. One [`BlobReader`] read copies at most
/// `MAX_READ_BYTES`, so this loops until the buffer is full or a read answers
/// `0`, and keeps what was filled.
pub(super) fn read_whole(blob: &Blob) -> Vec<u8> {
    let reader = BlobReader::open(blob);
    let mut bytes = vec![0; usize::try_from(reader.len()).unwrap_or(0)];

    let mut filled = 0;
    while filled < bytes.len() {
        let copied = reader.read_range(filled as u64, &mut bytes[filled..]);
        if copied == 0 {
            break;
        }
        filled += copied;
    }

    bytes.truncate(filled);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::MAX_READ_BYTES;

    #[test]
    fn a_blob_past_one_read_window_reads_whole() {
        let bytes = (0..=250).cycle().take(MAX_READ_BYTES + 5).collect::<Vec<u8>>();

        let read = read_whole(&Blob::from(bytes.clone()));

        assert_eq!(read, bytes);
    }
}
