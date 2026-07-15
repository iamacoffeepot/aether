//! The fail-closed prompt-manifest closure (ADR-0149 §The value vocabulary).
//!
//! An instruction slot is admissible only when it traces to a signed,
//! instruction-capable statement or a versioned policy artifact. These tests
//! exercise the reject and admit edges — the structural guard that ships
//! enforced from day one, independent of the stubbed signature mechanism.

#![allow(clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};

use aether_bloomery::{
    ClosureViolation, Digest, KeyProvider, Provenance, ProvenanceIndex, SignatureEnvelope, Slot, SlotRole, Statement,
    assemble_manifest,
};
use aether_bloomery::{FakeKeyProvider, KeyId, Observation};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

struct TestIndex {
    statements: HashMap<Digest, Statement>,
    policies: HashSet<Digest>,
}

impl ProvenanceIndex for TestIndex {
    fn statement(&self, digest: &Digest) -> Option<&Statement> {
        self.statements.get(digest)
    }

    fn is_versioned_policy(&self, digest: &Digest) -> bool {
        self.policies.contains(digest)
    }
}

/// A key provider that rejects every signature — for the unverified-signature
/// edge.
struct RejectingKeyProvider;

impl KeyProvider for RejectingKeyProvider {
    fn verify(&self, _envelope: &SignatureEnvelope, _message: &[u8]) -> bool {
        false
    }
}

fn signed_statement() -> Statement {
    Statement {
        words: b"do the thing".to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".into()),
            signature: vec![1, 2, 3],
        }),
        parents: vec![],
    }
}

fn observed_statement() -> Statement {
    Statement {
        words: b"seen elsewhere".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "github:issue/1".into() }),
        parents: vec![],
    }
}

fn instruction(artifact: Digest) -> Slot {
    Slot { artifact, role: SlotRole::Instruction, parent_closure: vec![] }
}

#[test]
fn instruction_slot_grounded_by_signed_statement_is_admissible() {
    let mut statements = HashMap::new();
    statements.insert(digest(1), signed_statement());
    let index = TestIndex { statements, policies: HashSet::new() };

    let manifest = assemble_manifest(vec![instruction(digest(1))], &index, &FakeKeyProvider).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn instruction_slot_grounded_by_policy_is_admissible() {
    let mut policies = HashSet::new();
    policies.insert(digest(2));
    let index = TestIndex { statements: HashMap::new(), policies };

    let manifest = assemble_manifest(vec![instruction(digest(2))], &index, &FakeKeyProvider).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn ungrounded_instruction_slot_is_rejected() {
    let index = TestIndex { statements: HashMap::new(), policies: HashSet::new() };

    let violation = assemble_manifest(vec![instruction(digest(3))], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(3) });
}

#[test]
fn observation_backed_instruction_slot_is_rejected() {
    let mut statements = HashMap::new();
    statements.insert(digest(4), observed_statement());
    let index = TestIndex { statements, policies: HashSet::new() };

    let violation = assemble_manifest(vec![instruction(digest(4))], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::NonAuthorInstruction { slot: digest(4) });
}

#[test]
fn unverified_signature_instruction_slot_is_rejected() {
    let mut statements = HashMap::new();
    statements.insert(digest(5), signed_statement());
    let index = TestIndex { statements, policies: HashSet::new() };

    let violation = assemble_manifest(vec![instruction(digest(5))], &index, &RejectingKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UnverifiedSignature { slot: digest(5) });
}

#[test]
fn ungrounded_context_slot_is_admissible() {
    // The closure gates only instruction slots; an ungrounded context slot is
    // fine — it is material, not command.
    let index = TestIndex { statements: HashMap::new(), policies: HashSet::new() };
    let context = Slot { artifact: digest(6), role: SlotRole::Context, parent_closure: vec![] };

    let manifest = assemble_manifest(vec![context], &index, &FakeKeyProvider).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}
