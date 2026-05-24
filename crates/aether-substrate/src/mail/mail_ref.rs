//! `MailRef` — the payload handle an actor inbox envelope carries
//! (ADR-0087).
//!
//! Phase 1 (iamacoffeepot/aether#1104) ships only the `Owned` variant: a
//! heap-owned byte buffer, byte-for-byte the `Vec<u8>` payload it
//! replaces, with no behaviour change. The zero-copy `InRing` variant —
//! a reference into a per-producer ring — lands in Phase 2
//! (iamacoffeepot/aether#1105) once the rings exist. `MailRef` is an
//! enum from the start (not a newtype) so every construction site
//! already reads `MailRef::Owned(..)` and the second variant slots in
//! without churning them; reads go through [`MailRef::bytes`] so the
//! `InRing` resolution lands in one place.

/// A handle to one mail's payload bytes carried on an actor inbox
/// envelope ([`crate::mail::registry::OwnedDispatch`]).
#[derive(Clone, Debug)]
pub enum MailRef {
    /// A heap-owned payload. The cross-boundary form (hub / MCP mail,
    /// substrate-generated mail) and — post-Phase-2 — the copy-out
    /// fallback when a producer ring is full.
    Owned(Box<[u8]>),
}

impl MailRef {
    /// Borrow the payload bytes for decoding. The single read accessor:
    /// Phase 2's `InRing` resolution is added here, so call sites that
    /// `decode_from_bytes(mail.bytes())` are unaffected by the variant
    /// gaining a second arm.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
        }
    }

    /// Consume into an owned `Vec<u8>`. For the few sites that move the
    /// payload into a downstream owned buffer or a test capture row.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Owned(bytes) => bytes.into_vec(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }
}

impl From<Vec<u8>> for MailRef {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Owned(bytes.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
