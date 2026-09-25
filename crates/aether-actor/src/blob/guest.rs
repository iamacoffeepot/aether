//! The guest's `Shared` blob backing (ADR-0238 decisions 2 and 4).
//!
//! When the engine delivers a `Shared` [`Blob`] into a wasm guest, the host
//! adds one count for its store entry to this instance's blob table, and the
//! guest's decoded value owns one `GuestHold` over that entry's hash. Reads
//! stream through the `blob_read_p32` import, and the drop of the value's last
//! clone calls `blob_drop_p32` once: `Blob` clones share one
//! `Arc<dyn BlobBacking>`, so there is no clone import and the host counts
//! grants, not clones.
//!
//! A negative status from `blob_len_p32` or `blob_read_p32` means the table
//! lost a count that a live `GuestHold` holds. That is a broken engine
//! invariant, not a guest error, so the SDK panics (ADR-0063 fail-fast).

use alloc::sync::Arc;

use aether_data::{Blob, BlobBacking, BlobHash};

use crate::wasm::bridge::blob as bridge;

/// One count on this instance's blob table for the entry `hash` names. Its
/// drop gives the count back.
struct GuestHold {
    hash: BlobHash,
    len: u64,
}

impl BlobBacking for GuestHold {
    fn len(&self) -> u64 {
        self.len
    }

    /// Copies at most `buf.len()` bytes, and at most `MAX_READ_BYTES`, since
    /// the host clamps each call; a caller loops until it sees `0`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize {
        let status = bridge::read(&self.hash, offset, buf);
        usize::try_from(status).unwrap_or_else(|_| lost_count("blob_read_p32", status))
    }
}

impl Drop for GuestHold {
    fn drop(&mut self) {
        bridge::drop_hold(&self.hash);
    }
}

/// The guest's gated grant: the `Shared` value delivery hands a guest for a
/// hash its instance's blob table holds, owning the one count delivery added.
/// `scripts/check-reference-mint.py` confines it.
///
/// # Panics
///
/// Panics when the host refuses `hash`: the table does not hold the count the
/// caller claims it added, which is an engine bug (ADR-0063 fail-fast).
#[doc(hidden)]
#[must_use]
pub fn __mint_guest_blob(hash: BlobHash) -> Blob {
    let status = bridge::len(&hash);
    let len = u64::try_from(status).unwrap_or_else(|_| lost_count("blob_len_p32", status));
    aether_data::__mint_shared_blob(Arc::new(GuestHold { hash, len }))
}

/// Fail fast on a refused hash that a live hold, or the grant about to build
/// one, holds a count for.
#[cold]
fn lost_count(import: &str, status: i64) -> ! {
    panic!(
        "aether-actor: {import} refused a blob this instance holds (status {status}); its blob table lost the count"
    );
}
