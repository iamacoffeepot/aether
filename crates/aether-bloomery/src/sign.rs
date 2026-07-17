//! Statement signing — the verification contract, not the custody (ADR-0149
//! §The value vocabulary, §The boundary).
//!
//! ADR-0149 ships the statement, manifest, and receipt shapes from the start
//! because everything downstream binds to them and they are what make replay
//! and audit possible. Verification rides the [`KeyProvider`] port so custody
//! can evolve behind it without a core change:
//!
//! - [`FakeKeyProvider`] — the always-valid stub, kept as a test / no-key
//!   helper (a solo bin with no adversary to reject).
//! - [`Ed25519KeyProvider`] — the real ed25519 verifier against an
//!   authorized-signer allowlist (ADR-0149 step 3). Native-only: the private
//!   keys and the allowlist live in the host's `aether.signing` capability
//!   (ADR-0150, credentials never leave the machine), and the wasm control
//!   actor holds no key material — so the concrete provider is constructed
//!   host-side, never in the guest.
//!
//! The fail-closed prompt closure ([`crate::manifest`]) is structural and
//! enforced from day one regardless of which provider is wired.

#[cfg(not(target_arch = "wasm32"))]
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[cfg(not(target_arch = "wasm32"))]
use ed25519_dalek::{Signature, VerifyingKey};
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

/// The real verifier: an ed25519 signature check against an in-memory
/// allowlist of authorized signers (ADR-0149 step 3). A signature verifies
/// only when its named [`KeyId`] is in the allowlist, its bytes are a 64-byte
/// ed25519 signature, and that signature checks against the allowlisted public
/// key over the exact `message` — every other case is a rejection, so the gate
/// is fail-closed. Custody of the allowlist and the private keys is the host's
/// `aether.signing` capability (ADR-0150, credentials never leave the machine);
/// this impl is pure, deterministic, and I/O-free — it belongs with the trait
/// it implements and is reused by the core manifest-closure gate's tests.
///
/// Native-only: the wasm control actor holds no key material and never
/// constructs a provider (ADR-0149 §The boundary), so `ed25519-dalek` stays out
/// of the wasm cdylib build.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct Ed25519KeyProvider {
    allowlist: BTreeMap<KeyId, VerifyingKey>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Ed25519KeyProvider {
    /// Build a provider over the `KeyId → verifying-key` allowlist — the set of
    /// signers authorized to sign in a person's stead (ADR-0151 key policy). An
    /// empty allowlist verifies nothing.
    #[must_use]
    pub fn new(allowlist: BTreeMap<KeyId, VerifyingKey>) -> Self {
        Self { allowlist }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl KeyProvider for Ed25519KeyProvider {
    fn verify(&self, envelope: &SignatureEnvelope, message: &[u8]) -> bool {
        let Some(key) = self.allowlist.get(&envelope.signer) else {
            return false;
        };
        let Ok(bytes) = <[u8; 64]>::try_from(envelope.signature.as_slice()) else {
            return false;
        };
        key.verify_strict(message, &Signature::from_bytes(&bytes)).is_ok()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use alloc::collections::BTreeMap;

    use ed25519_dalek::{Signer, SigningKey};

    use super::{Ed25519KeyProvider, KeyProvider, SignatureEnvelope};
    use crate::ids::KeyId;

    /// A deterministic signing key from a fixed 32-byte seed — no rng, so the
    /// test is reproducible.
    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// A provider trusting exactly `signer` at `key`'s public half.
    fn provider(signer: &str, key: &SigningKey) -> Ed25519KeyProvider {
        Ed25519KeyProvider::new(BTreeMap::from([(KeyId(signer.into()), key.verifying_key())]))
    }

    /// An author-signature envelope over `message` by `signer` using `key`.
    fn envelope(signer: &str, key: &SigningKey, message: &[u8]) -> SignatureEnvelope {
        SignatureEnvelope { signer: KeyId(signer.into()), signature: key.sign(message).to_bytes().to_vec() }
    }

    #[test]
    fn a_genuine_signature_by_an_allowlisted_signer_verifies() {
        let key = signing_key(7);
        let message = b"answer: choose A";
        assert!(provider("owner", &key).verify(&envelope("owner", &key, message), message));
    }

    #[test]
    fn a_signer_absent_from_the_allowlist_is_rejected() {
        // The signature is genuine, but "intruder" is not an authorized signer —
        // fail-closed on the allowlist, not just on the crypto.
        let key = signing_key(7);
        let message = b"do the thing";
        assert!(!provider("owner", &key).verify(&envelope("intruder", &key, message), message));
    }

    #[test]
    fn a_signature_by_the_wrong_key_is_rejected() {
        // Right signer id, but signed with a different key than the allowlist
        // holds — the ed25519 check fails.
        let message = b"do the thing";
        let bad = envelope("owner", &signing_key(9), message);
        assert!(!provider("owner", &signing_key(7)).verify(&bad, message));
    }

    #[test]
    fn tampered_words_do_not_verify() {
        // A signature over the original words does not verify over altered ones.
        let key = signing_key(7);
        let signed = envelope("owner", &key, b"answer: choose A");
        assert!(!provider("owner", &key).verify(&signed, b"answer: choose B"));
    }

    #[test]
    fn a_malformed_signature_is_rejected_not_a_panic() {
        // Signature bytes that are not 64 bytes long are refused at the length
        // check rather than panicking in the ed25519 decode.
        let key = signing_key(7);
        let message = b"do the thing";
        let malformed = SignatureEnvelope { signer: KeyId("owner".into()), signature: vec![1, 2, 3] };
        assert!(!provider("owner", &key).verify(&malformed, message));
    }
}
