//! The guest's `Shared` blob backing (ADR-0238 decisions 2, 3 and 4).
//!
//! When the engine delivers mail whose envelope attaches store entries, it
//! pins each one in this instance's blob table for the receive call. The
//! guest's decode reads each tag-1 field's hash and builds a `Shared` [`Blob`]
//! over it through [`GuestResolver`], and building the value takes one hold
//! (`blob_hold_p32`) that its `GuestHold` owns. Reads stream through the
//! `blob_read_p32` import, and the drop of the value's last clone gives the
//! hold back through `blob_drop_p32`: `Blob` clones share one
//! `Arc<dyn BlobBacking>`, so there is no clone import and the host's holds
//! equal live values, however many times a mail is decoded.
//!
//! A refused hold is a failed decode, not a panic: bytes a guest kept past
//! their receive call name a hash whose pin has gone. A negative status from
//! `blob_read_p32` on a live hold is different: the table lost a hold a live
//! `GuestHold` owns, a broken engine invariant, so the SDK panics (ADR-0063
//! fail-fast).

use alloc::sync::Arc;

use aether_data::{Blob, BlobBacking, BlobHash, wire};

use crate::wasm::bridge::blob as bridge;

/// One hold on this instance's blob table for the entry `hash` names. Its
/// drop gives the hold back.
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
        usize::try_from(status).unwrap_or_else(|_| lost_hold(status))
    }
}

impl Drop for GuestHold {
    fn drop(&mut self) {
        bridge::drop_hold(&self.hash);
    }
}

/// The guest's gated grant: takes one hold on `hash` and returns the `Shared`
/// value owning it, or `None` when the host refuses because this instance's
/// blob table neither pins nor holds the hash.
/// `scripts/check-reference-mint.py` confines it.
#[doc(hidden)]
#[must_use]
pub fn __mint_guest_blob(hash: BlobHash) -> Option<Blob> {
    let len = u64::try_from(bridge::hold(&hash)).ok()?;
    Some(aether_data::__mint_shared_blob(Arc::new(GuestHold { hash, len })))
}

/// Resolves a decode's tag-1 hashes by taking a hold on each. A decode that
/// fails partway drops the values it already built, and their holds go back.
pub(crate) struct GuestResolver;

impl wire::BlobResolver for GuestResolver {
    fn resolve(&mut self, hash: BlobHash) -> Result<Blob, wire::Error> {
        __mint_guest_blob(hash).ok_or(wire::Error::DetachedBlob(hash))
    }
}

/// Fail fast on a refused read of a hash a live hold owns.
#[cold]
fn lost_hold(status: i64) -> ! {
    panic!(
        "aether-actor: blob_read_p32 refused a blob this instance holds (status {status}); its blob table lost the hold"
    );
}
