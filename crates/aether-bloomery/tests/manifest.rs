//! The fail-closed prompt-manifest closure (ADR-0149 §The value vocabulary).
//!
//! An instruction slot is admissible only when it traces to a signed,
//! instruction-capable statement or a versioned policy artifact. These tests
//! exercise the reject and admit edges — the structural guard that ships
//! enforced from day one, independent of the stubbed signature mechanism.

#![allow(clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};

use aether_bloomery::{
    ClosureViolation, Digest, KeyProvider, MANIFEST_CLOSURE_BUDGET, Provenance, ProvenanceIndex, SignatureEnvelope,
    Slot, SlotRole, Statement, assemble_manifest,
};
use aether_bloomery::{FakeKeyProvider, KeyId, Observation};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

/// A distinct digest from a wide index — for chains longer than the 256 a
/// single seed byte can name.
fn digest_n(n: usize) -> Digest {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&(n as u64).to_le_bytes());
    Digest::from_bytes(bytes)
}

#[derive(Default)]
struct TestIndex {
    statements: HashMap<Digest, Statement>,
    policies: HashSet<Digest>,
    parents: HashMap<Digest, Vec<Digest>>,
}

impl ProvenanceIndex for TestIndex {
    fn statement(&self, digest: &Digest) -> Option<&Statement> {
        self.statements.get(digest)
    }

    fn is_versioned_policy(&self, digest: &Digest) -> bool {
        self.policies.contains(digest)
    }

    fn parents(&self, digest: &Digest) -> Option<Vec<Digest>> {
        self.parents.get(digest).cloned()
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
    let index = TestIndex { statements, policies: HashSet::new(), ..Default::default() };

    let manifest = assemble_manifest(vec![instruction(digest(1))], &index, &FakeKeyProvider).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn instruction_slot_grounded_by_policy_is_admissible() {
    let mut policies = HashSet::new();
    policies.insert(digest(2));
    let index = TestIndex { statements: HashMap::new(), policies, ..Default::default() };

    let manifest = assemble_manifest(vec![instruction(digest(2))], &index, &FakeKeyProvider).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn ungrounded_instruction_slot_is_rejected() {
    let index = TestIndex { statements: HashMap::new(), policies: HashSet::new(), ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(3))], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(3) });
}

#[test]
fn observation_backed_instruction_slot_is_rejected() {
    let mut statements = HashMap::new();
    statements.insert(digest(4), observed_statement());
    let index = TestIndex { statements, policies: HashSet::new(), ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(4))], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::NonAuthorInstruction { slot: digest(4) });
}

#[test]
fn unverified_signature_instruction_slot_is_rejected() {
    let mut statements = HashMap::new();
    statements.insert(digest(5), signed_statement());
    let index = TestIndex { statements, policies: HashSet::new(), ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(5))], &index, &RejectingKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UnverifiedSignature { slot: digest(5) });
}

#[test]
fn ungrounded_context_slot_is_admissible() {
    // The closure gates only instruction slots; an ungrounded context slot is
    // fine — it is material, not command.
    let index = TestIndex { statements: HashMap::new(), policies: HashSet::new(), ..Default::default() };
    let context = Slot { artifact: digest(6), role: SlotRole::Context, parent_closure: vec![] };

    let manifest = assemble_manifest(vec![context], &index, &FakeKeyProvider).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn derived_instruction_chain_is_admissible_and_authors_its_closure() {
    // A multi-hop instruction artifact (digest(10)) derives through digest(11)
    // to a signed statement at digest(12): the walk follows `parents` to the
    // ground and admits, and rewrites the slot's parent_closure to the ancestors
    // it actually traced — not whatever the caller declared.
    let mut statements = HashMap::new();
    statements.insert(digest(12), signed_statement());
    let mut parents = HashMap::new();
    parents.insert(digest(10), vec![digest(11)]);
    parents.insert(digest(11), vec![digest(12)]);
    let index = TestIndex { statements, parents, ..Default::default() };

    // The caller-declared closure is deliberately bogus; the assembler ignores it.
    let slot = Slot { artifact: digest(10), role: SlotRole::Instruction, parent_closure: vec![digest(99)] };
    let manifest = assemble_manifest(vec![slot], &index, &FakeKeyProvider).unwrap();

    assert_eq!(manifest.slots.len(), 1);
    assert_eq!(manifest.slots[0].parent_closure, vec![digest(11), digest(12)]);
}

#[test]
fn forged_parent_closure_does_not_ground_a_slot() {
    // The caller stuffs parent_closure with a signed statement's digest the
    // index does record — but `parents(slot.artifact)` records no such edge, so
    // grounding (which follows `parents`, never the slot field) never reaches
    // it. Tripwire on trusting the caller-declared closure.
    let mut statements = HashMap::new();
    statements.insert(digest(21), signed_statement());
    let index = TestIndex { statements, ..Default::default() };

    let slot = Slot { artifact: digest(20), role: SlotRole::Instruction, parent_closure: vec![digest(21)] };
    let violation = assemble_manifest(vec![slot], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(20) });
}

#[test]
fn broken_derivation_edge_is_rejected() {
    // The recorded chain leads to digest(31), a node the index does not know
    // (no statement, no policy, no parents), before reaching any ground — a
    // broken edge, refused.
    let mut parents = HashMap::new();
    parents.insert(digest(30), vec![digest(31)]);
    let index = TestIndex { parents, ..Default::default() };

    let slot = instruction(digest(30));
    let violation = assemble_manifest(vec![slot], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(30) });
}

#[test]
fn overlong_closure_exceeds_budget() {
    // A linear parent chain longer than the budget, grounding nowhere: the walk
    // refuses with ClosureBudgetExceeded rather than running unbounded.
    let mut parents = HashMap::new();
    for i in 0..(MANIFEST_CLOSURE_BUDGET + 2) {
        parents.insert(digest_n(i), vec![digest_n(i + 1)]);
    }
    let index = TestIndex { parents, ..Default::default() };

    let slot = Slot { artifact: digest_n(0), role: SlotRole::Instruction, parent_closure: vec![] };
    let violation = assemble_manifest(vec![slot], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::ClosureBudgetExceeded { slot: digest_n(0) });
}

#[test]
fn cyclic_parent_graph_terminates_and_rejects() {
    // A parent cycle is bounded by the visited-set: the walk terminates (this
    // test completing is the proof it does not loop) and refuses rather than
    // grounding, since no node in the cycle is a statement or policy.
    let mut parents = HashMap::new();
    parents.insert(digest(40), vec![digest(41)]);
    parents.insert(digest(41), vec![digest(42)]);
    parents.insert(digest(42), vec![digest(40)]);
    let index = TestIndex { parents, ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(40))], &index, &FakeKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(40) });
}
