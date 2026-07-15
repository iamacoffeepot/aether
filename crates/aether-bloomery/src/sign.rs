//! Statement signing — the shapes, not the custody (ADR-0149 §The value
//! vocabulary).
//!
//! ADR-0149 ships the statement, manifest, and receipt shapes from the
//! start because everything downstream binds to them and they are what make
//! replay and audit possible — but with a single operator there is no
//! second signer to defend against, so envelopes verify against a fake key
//! provider until real key custody (rotation, revocation, a signing console)
//! is a separate, later arc. The fail-closed prompt closure ([`crate::manifest`])
//! is structural and enforced from day one regardless.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::ids::KeyId;

/// An author signature over exact bytes: the assertion "this signer asserted
/// these bytes for this purpose". The only provenance that can become
/// *instruction* (ADR-0149 §The value vocabulary).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    /// The asserting signer's key identity.
    pub signer: KeyId,
    /// The signature bytes over the signed message. Opaque to the core — a
    /// [`KeyProvider`] interprets them.
    pub signature: Vec<u8>,
}

/// Verifies author signatures. The private keys never enter this crate (nor
/// wasm, in the actor slice) — the host's `signing` capability owns custody
/// (ADR-0149 §The boundary); this trait is the pure verification contract
/// the reducer and manifest assembly call through.
pub trait KeyProvider {
    /// Does `envelope` verify as a signature by its named signer over
    /// `message`?
    fn verify(&self, envelope: &SignatureEnvelope, message: &[u8]) -> bool;
}

/// The v1 stub: every well-formed envelope verifies. With one operator there
/// is no adversary to reject, so this stands in until real key custody lands
/// (ADR-0149 §The value vocabulary). Downstream code binds to the trait, so
/// swapping in a real provider is a host wiring change, not a core change.
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeKeyProvider;

impl KeyProvider for FakeKeyProvider {
    fn verify(&self, _envelope: &SignatureEnvelope, _message: &[u8]) -> bool {
        true
    }
}
