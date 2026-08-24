//! Statements and their provenance (ADR-0149 §The value vocabulary).
//!
//! A statement is an artifact carrying words plus one of three provenance
//! claims. Only an *author signature* can become instruction; an
//! *observation attestation* is context, never command; a *stage receipt*
//! records that a configured agent profile ran one process over exact
//! inputs and produced exact outputs.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};
use crate::ids::{KeyId, StageId};
use crate::sign::{AuthorityDoor, KeyProvider, SignatureEnvelope, authorization_message, sign_authorization};

/// An artifact carrying words plus exactly one provenance claim.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Statement {
    /// The asserted bytes — the exact bytes the provenance claim is over.
    pub words: Vec<u8>,
    /// How these words are grounded.
    pub provenance: Provenance,
    /// The parents in the derivation DAG this statement builds on.
    ///
    /// Outside the signature, and deliberately so: whoever holds an envelope can
    /// rewrite this without disturbing it. Authorization is bound inside the
    /// signed bytes instead ([`verify_authority`](Self::verify_authority),
    /// ADR-0182), so a structural check over `parents` is a key-free re-check of
    /// something the signature already fixed — never the thing establishing it.
    pub parents: Vec<Digest>,
}

impl Statement {
    /// Can these words become *instruction*? Only an author signature
    /// carries that authority (ADR-0149 §The value vocabulary).
    #[must_use]
    pub const fn is_instruction_capable(&self) -> bool {
        matches!(self.provenance, Provenance::AuthorSignature(_))
    }

    /// The key identity that asserted these words, when the provenance is an
    /// author signature.
    ///
    /// The claimed signer, not a verified one: reading it proves nothing on its
    /// own. It is what a caller needs *after*
    /// [`verify_authority`](Self::verify_authority) has held, to ask the key
    /// policy a second question the signature cannot answer — how high this
    /// signer may approve ([`KeyProvider::tier_ceiling`], #5324).
    #[must_use]
    pub const fn author_signer(&self) -> Option<&KeyId> {
        match &self.provenance {
            Provenance::AuthorSignature(envelope) => Some(&envelope.signer),
            Provenance::ObservationAttestation(_) | Provenance::StageReceipt(_) => None,
        }
    }

    /// Verify the statement's provenance as authority for one exact request.
    ///
    /// An author signature verifies its envelope over
    /// [`authorization_message(door, binding, &self.words)`](authorization_message)
    /// — the door and the request digest are inside the signed bytes alongside
    /// the words (ADR-0182), so a signature authorizes one request at one door
    /// and nothing else. The two non-author claims carry no signature and so
    /// never verify as *authority* — they are context, not command, and this
    /// returns `false` for them.
    ///
    /// `binding` is a required parameter with no default, and this crate offers
    /// no verification path over the words alone — so a new door cannot reach an
    /// unbound check through `Statement`, and adding one means writing a door
    /// and a binding at the call site rather than omitting them. That is a
    /// compile-time guarantee about *this* entry point, not about signature
    /// checking in general: [`KeyProvider::verify`] is public and takes an
    /// arbitrary `&[u8]`, so a caller that goes around this method can still
    /// verify whatever message it likes — and [`FakeKeyProvider`] accepts every
    /// message it is given. What is enforced is that nothing reaches a
    /// `Statement`'s *authority* without naming a door and a binding.
    ///
    /// [`FakeKeyProvider`]: crate::FakeKeyProvider
    ///
    /// [`parents`](Self::parents) is *not* consulted here. It stays the
    /// derivation-DAG provenance ADR-0149 gives it, and the structural checks
    /// callers run over it stand in addition to this one, never in place of it —
    /// the reducer holds no key material, so its parent scan is the only binding
    /// it can evaluate on replay.
    #[must_use]
    pub fn verify_authority(&self, keys: &dyn KeyProvider, door: AuthorityDoor, binding: Digest) -> bool {
        match &self.provenance {
            Provenance::AuthorSignature(envelope) => {
                keys.verify(envelope, authorization_message(door, binding, &self.words).as_bytes())
            }
            Provenance::ObservationAttestation(_) | Provenance::StageReceipt(_) => false,
        }
    }
}

impl ContentAddressed for Statement {
    const DOMAIN: &'static str = "aether.bloomery.statement";
}

/// One of the three provenance claims a [`Statement`] can carry.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Provenance {
    /// A person asserted these exact bytes for this purpose — the only claim
    /// that can become instruction.
    AuthorSignature(SignatureEnvelope),
    /// An adapter saw these bytes elsewhere — context, never command.
    ObservationAttestation(Observation),
    /// A configured agent profile ran one process over exact inputs and
    /// produced exact outputs.
    StageReceipt(StageReceipt),
}

/// The Approve door's exact statement shape, in one place (ADR-0182,
/// ADR-0207): an author signature over `scope`'s raw bytes, bound to `scope`.
///
/// `words` is the revision digest itself, which is what
/// [`Statement::verify_authority`] re-checks against the binding and what the
/// coordinator's approval reader matches an approval to its revision by.
/// `parents` is empty: authorization lives inside the signed bytes, not in the
/// derivation edge.
///
/// Deterministic, so re-running an amendment re-mints a byte-identical
/// statement with a byte-identical address and the store's duplicate check
/// makes the re-submit a no-op.
///
/// Native-only, like [`sign_authorization`] — the private half of key custody
/// is the operator's.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn signed_approval(signer: KeyId, seed: &[u8; 32], scope: Digest) -> Statement {
    let words = scope.as_bytes().to_vec();
    let envelope = sign_authorization(signer, seed, AuthorityDoor::Approve, scope, &words);
    Statement { words, provenance: Provenance::AuthorSignature(envelope), parents: Vec::new() }
}

/// The Cancel door's exact statement shape, in one place (ADR-0182): an author
/// signature over `intent`'s raw bytes, bound to `intent`.
///
/// `words` is the intent digest itself, which is what
/// [`Statement::verify_authority`] re-checks against the binding and what the
/// coordinator's cancel route matches a cancel to its commission by.
/// `parents` is empty: authorization lives inside the signed bytes, not in the
/// derivation edge.
///
/// Deterministic, so re-running a cancel re-mints a byte-identical statement
/// with a byte-identical address and the store's not-open refusal is the only
/// thing a second attempt hits.
///
/// Native-only, like [`sign_authorization`] — the private half of key custody
/// is the operator's.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn signed_cancel(signer: KeyId, seed: &[u8; 32], intent: Digest) -> Statement {
    let words = intent.as_bytes().to_vec();
    let envelope = sign_authorization(signer, seed, AuthorityDoor::Cancel, intent, &words);
    Statement { words, provenance: Provenance::AuthorSignature(envelope), parents: Vec::new() }
}

/// The Reopen door's exact statement shape, in one place (ADR-0182): an author
/// signature over `intent`'s raw bytes, bound to `intent`.
///
/// The same shape [`signed_cancel`] carries, at its own door. Sharing the Cancel
/// door would make one signature good for both a retirement and a restoration,
/// and those are opposite acts an operator signs separately.
///
/// Deterministic, so a re-run re-mints a byte-identical statement and the
/// store's already-open refusal is the only thing a second attempt hits.
///
/// Native-only, like [`sign_authorization`] — the private half of key custody
/// is the operator's.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn signed_reopen(signer: KeyId, seed: &[u8; 32], intent: Digest) -> Statement {
    let words = intent.as_bytes().to_vec();
    let envelope = sign_authorization(signer, seed, AuthorityDoor::Reopen, intent, &words);
    Statement { words, provenance: Provenance::AuthorSignature(envelope), parents: Vec::new() }
}

/// Where an adapter saw the observed bytes. An observation carries no
/// authority — it becomes intent only when a person adopts its exact digest
/// in a native signed statement (ADR-0149 §The boundary, second amendment).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Observation {
    /// A stable, human-readable source label (e.g. the adapter and the
    /// external object it mirrored).
    pub source: String,
}

/// A record that one stage binding ran: the profile, the exact inputs it
/// consumed, and the exact outputs it produced (ADR-0149 §The line).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageReceipt {
    /// The stage that ran.
    pub stage: StageId,
    /// The exact [`AgentProfile`] that ran, attested by digest — the value
    /// that makes "a configured agent profile ran one process" verifiable: a
    /// reader recomputes the profile's address to confirm the configuration.
    /// *Who* ran is not stored: the `iama-{stage}` worker identity is derived
    /// from [`stage`](Self::stage) via [`StageId::worker_identity`].
    ///
    /// [`AgentProfile`]: crate::values::AgentProfile
    pub profile: Digest,
    /// The exact inputs consumed, by digest.
    pub inputs: Vec<Digest>,
    /// The exact outputs produced, by digest.
    pub outputs: Vec<Digest>,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::string::String;

    use ed25519_dalek::SigningKey;

    use super::{signed_cancel, signed_reopen};
    use crate::digest::Digest;
    use crate::ids::KeyId;
    use crate::sign::{AuthorityDoor, AuthorizedSigner, Ed25519KeyProvider};
    use crate::values::Tier;

    #[test]
    fn a_signed_cancel_verifies_only_at_the_cancel_door_for_its_own_intent() {
        let seed = [7_u8; 32];
        let key = SigningKey::from_bytes(&seed);
        let signer = KeyId(String::from("operator"));
        let keys = Ed25519KeyProvider::new(BTreeMap::from([(
            signer.clone(),
            AuthorizedSigner { key: key.verifying_key(), ceiling: Tier::Human },
        )]));
        let intent = Digest::from_bytes([3; 32]);
        let other = Digest::from_bytes([4; 32]);
        let statement = signed_cancel(signer, &seed, intent);

        assert!(
            statement.verify_authority(&keys, AuthorityDoor::Cancel, intent),
            "a cancel must verify at the Cancel door over its own intent"
        );
        assert!(
            !statement.verify_authority(&keys, AuthorityDoor::Approve, intent),
            "a cancel minted at the wrong door would be a signature good for a commission its signer never read"
        );
        assert!(
            !statement.verify_authority(&keys, AuthorityDoor::Cancel, other),
            "a cancel bound to nothing would be a signature good for a commission its signer never read"
        );
    }

    #[test]
    fn a_reopen_and_a_cancel_are_not_each_other_at_their_own_doors() {
        // Retiring a commission and putting it back in the line are opposite
        // acts. A signature good for both would let one submitted envelope be
        // replayed at the other door.
        let seed = [7_u8; 32];
        let key = SigningKey::from_bytes(&seed);
        let signer = KeyId(String::from("operator"));
        let keys = Ed25519KeyProvider::new(BTreeMap::from([(
            signer.clone(),
            AuthorizedSigner { key: key.verifying_key(), ceiling: Tier::Human },
        )]));
        let intent = Digest::from_bytes([3; 32]);

        let reopen = signed_reopen(signer.clone(), &seed, intent);
        let cancel = signed_cancel(signer, &seed, intent);

        assert!(
            reopen.verify_authority(&keys, AuthorityDoor::Reopen, intent),
            "a reopen must verify at the Reopen door over its own intent"
        );
        assert!(!reopen.verify_authority(&keys, AuthorityDoor::Cancel, intent), "a reopen must not retire anything");
        assert!(!cancel.verify_authority(&keys, AuthorityDoor::Reopen, intent), "a cancel must not restore anything");
    }
}
