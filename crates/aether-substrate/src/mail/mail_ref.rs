//! `MailRef` — the payload handle an actor inbox envelope carries
//! (ADR-0087).
//!
//! Two payload forms:
//!
//! - **`Owned`** — a heap `Box<[u8]>`. The cross-boundary form (hub / MCP
//!   mail, substrate-generated mail) and the copy-out fallback when a
//!   producer ring is full. Byte-for-byte the `Vec<u8>` it replaced in
//!   Phase 1 (iamacoffeepot/aether#1104).
//! - **`InRing`** — a zero-copy reference into a per-producer
//!   [`MailRing`] (Phase 2, iamacoffeepot/aether#1105): the producing
//!   actor buffered this mail's bytes into its ring as one mail of a
//!   blob, and the recipient reads them in place. The ref carries an
//!   `Arc<MailRing>`, so the ring outlives every in-flight ref by
//!   refcount alone — no registry, no resolve-after-drop window.
//!
//! # Lock lifecycle is RAII on the ref
//!
//! Each `InRing` ref owns exactly one count of its blob's reclaim lock:
//! [`push_blob`](MailRing::push_blob) sets the lock to the blob's mail
//! count, and one ref is minted per mail ([`MailRef::in_ring`], which
//! does *not* touch the lock — it is pre-counted). The ref releases its
//! count on `Drop` and acquires another on `Clone`, so the count tracks
//! the number of live refs no matter how mail is moved, cloned, or
//! dropped-unread. The producer reclaims the region once the count hits
//! zero. This is why [`MailRef::bytes`] can hand back the in-ring slice
//! safely: the borrow is tied to `&self`, and `self` holds the region
//! live for its whole lifetime.

use std::fmt;
use std::sync::Arc;

use crate::mail::ring::{MailLoc, MailRing};

/// A handle to one mail's payload bytes carried on an actor inbox
/// envelope ([`crate::mail::registry::OwnedDispatch`]).
pub enum MailRef {
    /// A heap-owned payload (cross-boundary mail, or the ring-full
    /// copy-out fallback).
    Owned(Box<[u8]>),
    /// A zero-copy reference into a per-producer ring. Holds one count of
    /// the blob's reclaim lock for its lifetime (see the module docs).
    InRing {
        ring: Arc<MailRing>,
        header_off: u32,
        payload_off: u32,
        len: u32,
    },
}

impl MailRef {
    /// Mint an `InRing` ref for one mail of a just-written blob. Does
    /// **not** touch the lock — [`MailRing::push_blob`] already counted
    /// this ref. Cloning the returned ref acquires another count; dropping
    /// it releases one.
    #[must_use]
    pub fn in_ring(ring: Arc<MailRing>, loc: MailLoc) -> Self {
        Self::InRing {
            ring,
            header_off: loc.header_off,
            payload_off: loc.payload_off,
            len: loc.len,
        }
    }

    /// Borrow the payload bytes for decoding. Works for both variants:
    /// `Owned` borrows the box, `InRing` borrows the ring region (kept
    /// live by this ref's held lock). The borrow is tied to `&self`.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::InRing {
                ring,
                payload_off,
                len,
                ..
            } => {
                // SAFETY: this ref holds one count of the blob lock for its
                // whole lifetime, so the producer cannot reclaim/overwrite
                // the region while the returned borrow (tied to `&self`) is
                // live.
                unsafe { ring.payload(*payload_off, *len) }
            }
        }
    }

    /// Copy the payload into an owned `Vec<u8>`. For the few sites that
    /// move the payload into a downstream owned buffer or a test capture
    /// row. (For `InRing` this necessarily copies; the ref's lock is
    /// released when `self` drops at the end of this call.)
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes().to_vec()
    }

    /// Materialize into the `Owned` variant, releasing any ring lock.
    /// `Owned` is returned unchanged (no copy); `InRing` copies its
    /// region out to a heap buffer and drops, freeing the ring region.
    /// Used where a mail may be held for an unbounded window — parked on
    /// a missing handle, or queued for cross-engine egress — so it never
    /// pins a producer ring region for that whole time (2b).
    #[must_use]
    pub fn into_owned(self) -> Self {
        match self {
            owned @ Self::Owned(_) => owned,
            in_ring => Self::Owned(in_ring.bytes().to_vec().into_boxed_slice()),
        }
    }

    /// Payload length in bytes. Reads the `InRing` length field directly —
    /// no region access — so it is valid even without holding the borrow.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.len(),
            Self::InRing { len, .. } => *len as usize,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Clone for MailRef {
    fn clone(&self) -> Self {
        match self {
            Self::Owned(bytes) => Self::Owned(bytes.clone()),
            Self::InRing {
                ring,
                header_off,
                payload_off,
                len,
            } => {
                // A clone is a new live holder of the region — acquire
                // another lock count so reclaim waits for it too.
                // SAFETY: `self` already holds a count, keeping the blob
                // live across the increment.
                unsafe { ring.acquire(*header_off) };
                Self::InRing {
                    ring: Arc::clone(ring),
                    header_off: *header_off,
                    payload_off: *payload_off,
                    len: *len,
                }
            }
        }
    }
}

impl Drop for MailRef {
    fn drop(&mut self) {
        if let Self::InRing {
            ring, header_off, ..
        } = self
        {
            // Release the one lock count this ref held; the producer
            // reclaims the blob once the count reaches zero.
            // SAFETY: this ref held exactly one count for `header_off`.
            unsafe { ring.release(*header_off) };
        }
    }
}

impl fmt::Debug for MailRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owned(bytes) => f.debug_tuple("Owned").field(&bytes.len()).finish(),
            Self::InRing {
                header_off,
                payload_off,
                len,
                ..
            } => f
                .debug_struct("InRing")
                .field("header_off", header_off)
                .field("payload_off", payload_off)
                .field("len", len)
                .finish(),
        }
    }
}

impl From<Vec<u8>> for MailRef {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Owned(bytes.into_boxed_slice())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test code: push_blob unwraps assert via panic on a sized ring"
)]
mod tests {
    use super::*;
    use crate::mail::ring::OutMail;

    #[test]
    fn owned_round_trips_bytes_and_vec() {
        let r = MailRef::from(vec![1u8, 2, 3, 4]);
        assert_eq!(r.bytes(), &[1, 2, 3, 4]);
        assert_eq!(r.len(), 4);
        assert!(!r.is_empty());
        assert_eq!(r.into_vec(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn empty_owned_is_empty() {
        let r = MailRef::from(Vec::new());
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert_eq!(r.bytes(), &[] as &[u8]);
    }

    #[test]
    fn in_ring_reads_in_place_and_releases_on_drop() {
        let ring = Arc::new(MailRing::with_capacity(1024));
        let locs = ring
            .push_blob(&[OutMail {
                recipient: 1,
                kind: 2,
                payload: &[10, 20, 30],
            }])
            .unwrap();
        let live = ring.live_bytes();
        assert!(live > 0);
        let r = MailRef::in_ring(Arc::clone(&ring), locs[0]);
        assert_eq!(r.bytes(), &[10, 20, 30]);
        assert_eq!(r.len(), 3);
        // lock still held -> nothing reclaims
        assert_eq!(ring.reclaim(), 0);
        assert_eq!(ring.live_bytes(), live);
        drop(r);
        // ref dropped -> lock released -> reclaimable
        assert_eq!(ring.reclaim(), live);
        assert_eq!(ring.live_bytes(), 0);
    }

    #[test]
    fn in_ring_clone_holds_region_until_both_drop() {
        let ring = Arc::new(MailRing::with_capacity(1024));
        let locs = ring
            .push_blob(&[OutMail {
                recipient: 1,
                kind: 2,
                payload: &[7; 16],
            }])
            .unwrap();
        let live = ring.live_bytes();
        let a = MailRef::in_ring(Arc::clone(&ring), locs[0]);
        let b = a.clone();
        drop(a);
        // clone still holds a count -> not reclaimable
        assert_eq!(ring.reclaim(), 0);
        assert_eq!(b.bytes(), &[7u8; 16]);
        drop(b);
        assert_eq!(ring.reclaim(), live);
    }

    #[test]
    fn in_ring_into_vec_copies_then_releases() {
        let ring = Arc::new(MailRing::with_capacity(1024));
        let locs = ring
            .push_blob(&[OutMail {
                recipient: 9,
                kind: 9,
                payload: &[1, 2, 3, 4, 5],
            }])
            .unwrap();
        let live = ring.live_bytes();
        let r = MailRef::in_ring(Arc::clone(&ring), locs[0]);
        let v = r.into_vec();
        assert_eq!(v, vec![1, 2, 3, 4, 5]);
        // into_vec consumed the ref -> released -> reclaimable
        assert_eq!(ring.reclaim(), live);
    }
}
