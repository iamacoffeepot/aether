//! The guest's `Shared` blob backing (ADR-0238 decisions 2, 3 and 4).
//!
//! When the engine delivers mail whose envelope attaches store entries, it
//! pins each one in this instance's blob table for the receive call. The
//! guest's decode reads each tag-1 field's hash and builds a `Shared` [`Blob`]
//! over it through `GuestResolver`, and building the value takes one hold
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
//!
//! A guest's sends encode through [`encode_guest`] (ADR-0238 decisions 3 and
//! 12): a held `Shared` value is written as tag 1 and its hash, and a clone of
//! it rides the send as [`EncodedGuestMail::keep`], so the hold stays live
//! while the send needs it. For mail to the host, the host's resolve on send
//! finds each hash among this instance's holds and attaches the entry, and
//! the recipient shares the bytes instead of receiving a copy read out
//! through `blob_read_p32`. For mail that stays inside the cluster, the queued
//! mail keeps the clones until the inline recipient's dispatch has run, so
//! its decode is admitted by a live hold even when the sender dropped its own
//! value first. Every other value, an `Owned` one included, is written as
//! tag 0 and its bytes.
//!
//! The backing and the grant exist only on wasm32. The host build of the SDK
//! never holds a guest blob, so there [`encode_guest`] is the plain encode
//! with nothing kept.

use alloc::vec::Vec;

use aether_data::{Blob, Kind};
#[cfg(target_arch = "wasm32")]
use {
    crate::wasm::bridge::blob as bridge,
    aether_data::{BlobBacking, BlobHash, wire},
    alloc::sync::Arc,
    core::any::Any,
};

/// The tag a `Blob` field carries when it names a held value by hash.
#[cfg(target_arch = "wasm32")]
const TAG_HASH: u8 = 1;

/// A guest send's payload: the bytes, and the held values it names by hash.
/// Keeping the values keeps their holds, for as long as the send needs them:
/// the host call that resolves them, or the queued intra-cluster mail until
/// its recipient has dispatched.
pub struct EncodedGuestMail {
    pub bytes: Vec<u8>,
    pub keep: Vec<Blob>,
}

impl EncodedGuestMail {
    /// Bytes that name no value this send keeps: a raw forward, or a batch of
    /// cast payloads.
    pub const fn plain(bytes: Vec<u8>) -> Self {
        Self { bytes, keep: Vec::new() }
    }
}

/// Writes a held `Shared` value (a `GuestHold` backing) as tag 1 and its hash
/// and keeps a clone; anything else as tag 0 and its bytes.
#[cfg(target_arch = "wasm32")]
pub struct GuestEncoder {
    out: Vec<u8>,
    keep: Vec<Blob>,
}

#[cfg(target_arch = "wasm32")]
impl wire::Encoder for GuestEncoder {
    fn out(&mut self) -> &mut Vec<u8> {
        &mut self.out
    }

    fn blob(&mut self, value: &Blob) -> Result<(), wire::Error> {
        let Some(hold) = held(value) else {
            return wire::Encoder::blob(&mut self.out, value);
        };
        self.out.push(TAG_HASH);
        self.out.extend_from_slice(hold.hash.as_bytes());
        self.keep.push(value.clone());
        Ok(())
    }
}

/// Encode `payload` for a guest send: see [`GuestEncoder`].
///
/// # Panics
///
/// When a length exceeds the `u32` ceiling, the one way a wire encode fails.
#[cfg(target_arch = "wasm32")]
pub fn encode_guest<K: Kind>(payload: &K) -> EncodedGuestMail {
    let mut encoder = GuestEncoder { out: Vec::new(), keep: Vec::new() };
    payload.encode_with(&mut encoder).expect("wire encode to Vec fails only past the u32 length ceiling");
    EncodedGuestMail { bytes: encoder.out, keep: encoder.keep }
}

/// Encode `payload` for a guest send. The host build holds no guest blob, so
/// it is the plain encode with nothing kept.
#[cfg(not(target_arch = "wasm32"))]
pub fn encode_guest<K: Kind>(payload: &K) -> EncodedGuestMail {
    EncodedGuestMail::plain(payload.encode_into_bytes())
}

/// The hold behind `value` when it is a guest `Shared` value, recovered by
/// downcast. `None` for `Owned` bytes.
#[cfg(target_arch = "wasm32")]
fn held(value: &Blob) -> Option<&GuestHold> {
    let backing: &dyn Any = aether_data::__shared_backing(value)?.as_ref();
    backing.downcast_ref::<GuestHold>()
}

/// One hold on this instance's blob table for the entry `hash` names. Its
/// drop gives the hold back.
#[cfg(target_arch = "wasm32")]
struct GuestHold {
    hash: BlobHash,
    len: u64,
}

#[cfg(target_arch = "wasm32")]
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

#[cfg(target_arch = "wasm32")]
impl Drop for GuestHold {
    fn drop(&mut self) {
        bridge::drop_hold(&self.hash);
    }
}

/// The guest's gated grant: takes one hold on `hash` and returns the `Shared`
/// value owning it, or `None` when the host refuses because this instance's
/// blob table neither pins nor holds the hash.
/// `scripts/check-reference-mint.py` confines it.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
#[must_use]
pub fn __mint_guest_blob(hash: BlobHash) -> Option<Blob> {
    let len = u64::try_from(bridge::hold(&hash)).ok()?;
    Some(aether_data::__mint_shared_blob(Arc::new(GuestHold { hash, len })))
}

/// Resolves a decode's tag-1 hashes by taking a hold on each. A decode that
/// fails partway drops the values it already built, and their holds go back.
#[cfg(target_arch = "wasm32")]
pub struct GuestResolver;

#[cfg(target_arch = "wasm32")]
impl wire::BlobResolver for GuestResolver {
    fn resolve(&mut self, hash: BlobHash) -> Result<Blob, wire::Error> {
        __mint_guest_blob(hash).ok_or(wire::Error::DetachedBlob(hash))
    }
}

/// Fail fast on a refused read of a hash a live hold owns.
#[cfg(target_arch = "wasm32")]
#[cold]
fn lost_hold(status: i64) -> ! {
    panic!(
        "aether-actor: blob_read_p32 refused a blob this instance holds (status {status}); its blob table lost the hold"
    );
}

/// Test support: a `Shared` value whose backing reports its own drop, so a
/// host test can watch how long a send keeps a value alive. Minted here
/// because the grant is confined to this file.
#[cfg(test)]
pub mod tracked {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use aether_data::{Blob, BlobBacking};

    /// Sets its flag when the last clone of the value over it drops.
    struct Tracked(Arc<AtomicBool>);

    impl BlobBacking for Tracked {
        fn len(&self) -> u64 {
            0
        }

        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> usize {
            0
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// An empty `Shared` value, and the flag its backing sets when dropped.
    pub fn tracked_blob() -> (Blob, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        (aether_data::__mint_shared_blob(Arc::new(Tracked(Arc::clone(&dropped)))), dropped)
    }
}
