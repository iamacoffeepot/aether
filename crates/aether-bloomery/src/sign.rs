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
//!
//! # What a signature covers
//!
//! Not the words alone. [`authorization_message`] is the message every author
//! signature verifies against: the digest of an [`AuthorityDoor`], the request
//! digest the signature is bound to, and the asserted words together (ADR-0182).
//! Putting the binding inside the signed bytes is what makes a captured envelope
//! authorize the one request it was made for — a statement's `parents` sit
//! outside the signature and can be rewritten by whoever holds it, so a door
//! that bound structurally alone turned one legitimate authorization into a
//! standing credential.

#[cfg(not(target_arch = "wasm32"))]
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[cfg(not(target_arch = "wasm32"))]
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::KeyId;

/// Which door an author signature authorizes (ADR-0182).
///
/// A closed enum hashed into the signed subject, so an envelope minted for one
/// door never verifies at another even where the words and the binding coincide.
/// Adding a door is a variant, and the variant is what keeps its envelopes
/// separate from every existing door's.
///
/// **Variant order is part of the signed subject.** The discriminant is what
/// serde encodes into the authorization [`authorization_message`] hashes, so
/// reordering these variants — or inserting one anywhere but the end — silently
/// changes the message every past signature was minted over and invalidates all
/// of them, with no compile error and no decode failure to announce it. Append
/// new doors; never reorder.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AuthorityDoor {
    /// Approving a member's scope revision at seal time, bound to that
    /// revision's digest.
    Approve,
    /// Adopting an answer to a parked question, bound to the question's digest.
    Answer,
    /// Releasing an orphaned claim ref, bound to the release request's digest
    /// (ADR-0179).
    OrphanClaimRelease,
    /// Grounding a prompt-manifest instruction slot, bound to the artifact the
    /// signature grounds. Not a request door: nothing acts on it, so it
    /// authorizes no mutation — the variant exists so the closure walk's
    /// cryptographic check cannot borrow a request door's envelope, and
    /// [`ground_instruction`](crate::manifest) enforces that by grounding only
    /// on a recorded authority whose door is this one.
    Ground,
    /// Cancelling an open commission, bound to that commission's intent digest.
    /// Appended past [`Self::Ground`] so existing door discriminants stay put.
    Cancel,
    /// Accepting an architecture decision record, bound to that ADR's digest.
    /// Appended past [`Self::Cancel`] so existing door discriminants stay put.
    Accept,
}

/// The subject an author signature actually covers (ADR-0182): the door, the
/// request it binds, and the asserted words together.
///
/// Private because it is a hashing subject rather than a stored value — callers
/// reach it through [`authorization_message`], which is the only way to produce
/// the message a [`KeyProvider`] verifies against.
#[derive(Serialize)]
struct Authorization<'a> {
    /// Which door this signature authorizes.
    door: AuthorityDoor,
    /// The exact request digest this signature is good for.
    binding: Digest,
    /// The asserted bytes.
    words: &'a [u8],
}

impl ContentAddressed for Authorization<'_> {
    const DOMAIN: &'static str = "aether.bloomery.authorization";
}

/// The message an author signature over `words` at `door` bound to `binding`
/// must cover (ADR-0182).
///
/// The binding is a parameter with no default and there is no verification path
/// over words alone, so a signature authorizes exactly one request at exactly
/// one door: rewriting a statement's `parents` changes the artifact's address
/// without producing a signature that verifies against the new target.
#[must_use]
pub fn authorization_message(door: AuthorityDoor, binding: Digest, words: &[u8]) -> Digest {
    digest_of(&Authorization { door, binding, words })
}

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

    use super::{AuthorityDoor, Ed25519KeyProvider, KeyProvider, SignatureEnvelope, authorization_message};
    use crate::digest::Digest;
    use crate::ids::KeyId;

    /// A distinct binding digest per seed — two requests to sign for.
    fn binding(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

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
    fn a_signature_minted_for_one_door_does_not_verify_at_another() {
        // The whole point of hashing AuthorityDoor into the subject (ADR-0182):
        // an answer envelope must not release an orphaned claim ref, even when
        // the operator signed the same words for the same request digest.
        let key = signing_key(7);
        let words = b"yes";
        let signed =
            envelope("owner", &key, authorization_message(AuthorityDoor::Answer, binding(1), words).as_bytes());

        assert!(
            !provider("owner", &key).verify(
                &signed,
                authorization_message(AuthorityDoor::OrphanClaimRelease, binding(1), words).as_bytes()
            )
        );
    }

    #[test]
    fn a_signature_minted_for_one_binding_does_not_verify_at_another() {
        // The replay this ADR closes: re-pointing a captured envelope at a
        // second request digest yields no verifying signature, so the constant
        // words a release door signs are no longer a standing credential.
        let key = signing_key(7);
        let words = b"release orphan bloomery claim";
        let signed = envelope(
            "owner",
            &key,
            authorization_message(AuthorityDoor::OrphanClaimRelease, binding(1), words).as_bytes(),
        );

        assert!(
            !provider("owner", &key).verify(
                &signed,
                authorization_message(AuthorityDoor::OrphanClaimRelease, binding(2), words).as_bytes()
            )
        );
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
