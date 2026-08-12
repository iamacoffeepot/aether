//! The fail-closed prompt-manifest closure (ADR-0149 §The value vocabulary).
//!
//! An instruction slot is admissible only when it traces to a signed,
//! instruction-capable statement or a versioned policy artifact. These tests
//! exercise the reject and admit edges — the structural guard that ships
//! enforced from day one, independent of the stubbed signature mechanism.

#![allow(clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};

use std::collections::BTreeMap;

use aether_bloomery::{
    AuthorityDoor, ClosureViolation, Digest, Ed25519KeyProvider, KeyProvider, MANIFEST_CLOSURE_BUDGET, Provenance,
    ProvenanceIndex, SignatureEnvelope, Slot, SlotRole, Statement, assemble_manifest, authorization_message,
};
use aether_bloomery::{KeyId, Observation};
use ed25519_dalek::{Signer, SigningKey};
use proptest::prelude::*;

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
    /// Statement digests the index deliberately records no authority for — the
    /// `authority_binding` → `None` case the fail-closed walk must refuse
    /// (ADR-0182).
    unbound: HashSet<Digest>,
    /// Authority records that differ from the default `(Ground, <node digest>)`.
    /// The default coincides with the node on both halves, which would let a
    /// walk that ignored this method entirely — hardcoding the door, the
    /// binding, or both — pass every test here. An override makes the recorded
    /// value observably different from the node, so the walk has to read it.
    bindings: HashMap<Digest, (AuthorityDoor, Digest)>,
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

    fn authority_binding(&self, digest: &Digest) -> Option<(AuthorityDoor, Digest)> {
        if self.unbound.contains(digest) {
            return None;
        }
        // A host grounds a statement at the artifact digest it recorded it
        // under, so `(Ground, node)` is the default record. `bindings` overrides
        // it where a test needs the record to disagree with the node.
        self.bindings
            .get(digest)
            .copied()
            .or_else(|| self.statements.contains_key(digest).then_some((AuthorityDoor::Ground, *digest)))
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

/// The deterministic authorized signer (fixed seed, no rng) — its public half
/// backs [`real_provider`] and its private half signs [`signed_statement`], so
/// the statement genuinely verifies against the provider.
fn owner_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// The real ed25519 provider trusting exactly the `owner` signer — the same
/// custody shape the host `aether.signing` capability constructs, so the
/// closure gate's tests run against the real verifier, not the fake stub.
fn real_provider() -> Ed25519KeyProvider {
    Ed25519KeyProvider::new(BTreeMap::from([(KeyId("owner".into()), owner_key().verifying_key())]))
}

/// A statement genuinely signed as authority to ground the artifact stored at
/// `at` — the message [`TestIndex::authority_binding`] reports for that node, so
/// the walk's recovered binding and the signed one agree (ADR-0182).
fn signed_statement(at: Digest) -> Statement {
    signed_at(AuthorityDoor::Ground, at)
}

/// A statement genuinely signed by the owner as authority for `door` bound to
/// `binding` — the knob the door / binding discrimination cases turn.
fn signed_at(door: AuthorityDoor, binding: Digest) -> Statement {
    let words = b"do the thing".to_vec();
    let signature = owner_key().sign(authorization_message(door, binding, &words).as_bytes()).to_bytes().to_vec();
    Statement {
        words,
        provenance: Provenance::AuthorSignature(SignatureEnvelope { signer: KeyId("owner".into()), signature }),
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
    statements.insert(digest(1), signed_statement(digest(1)));
    let index = TestIndex { statements, policies: HashSet::new(), ..Default::default() };

    let manifest = assemble_manifest(vec![instruction(digest(1))], &index, &real_provider()).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn instruction_slot_grounded_by_policy_is_admissible() {
    let mut policies = HashSet::new();
    policies.insert(digest(2));
    let index = TestIndex { statements: HashMap::new(), policies, ..Default::default() };

    let manifest = assemble_manifest(vec![instruction(digest(2))], &index, &real_provider()).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn ungrounded_instruction_slot_is_rejected() {
    let index = TestIndex { statements: HashMap::new(), policies: HashSet::new(), ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(3))], &index, &real_provider()).unwrap_err();
    assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(3) });
}

#[test]
fn observation_backed_instruction_slot_is_rejected() {
    let mut statements = HashMap::new();
    statements.insert(digest(4), observed_statement());
    let index = TestIndex { statements, policies: HashSet::new(), ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(4))], &index, &real_provider()).unwrap_err();
    assert_eq!(violation, ClosureViolation::NonAuthorInstruction { slot: digest(4) });
}

#[test]
fn unverified_signature_instruction_slot_is_rejected() {
    let mut statements = HashMap::new();
    statements.insert(digest(5), signed_statement(digest(5)));
    let index = TestIndex { statements, policies: HashSet::new(), ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(5))], &index, &RejectingKeyProvider).unwrap_err();
    assert_eq!(violation, ClosureViolation::UnverifiedSignature { slot: digest(5) });
}

#[test]
fn a_statement_the_index_records_no_authority_for_is_rejected() {
    // ADR-0182: verification needs a binding, and the walk has no request of its
    // own to supply one. When the index cannot say what the host verified a
    // statement against, the walk refuses rather than falling back to checking
    // the signature over the words alone — which is the unbound path this change
    // exists to remove. The signature itself is genuine and the provider is the
    // real one, so only the missing record can produce this refusal.
    let mut statements = HashMap::new();
    statements.insert(digest(7), signed_statement(digest(7)));
    let index = TestIndex { statements, unbound: HashSet::from([digest(7)]), ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(7))], &index, &real_provider()).unwrap_err();

    assert_eq!(violation, ClosureViolation::UnverifiedSignature { slot: digest(7) });
}

#[test]
fn the_walk_verifies_against_the_recorded_binding_not_the_node_digest() {
    // Tripwire: the recorded binding is the one the check runs under. Here the
    // index records digest(21) for the node digest(20) and the statement is
    // signed for digest(21), so the node digest and the signed binding are
    // different values — a walk that passed the node (or any other digest it
    // could reach without asking the index) would produce a different
    // authorization message and refuse. Every other case in this file records
    // `(Ground, node)`, where those two coincide and nothing is distinguished.
    let mut statements = HashMap::new();
    statements.insert(digest(20), signed_at(AuthorityDoor::Ground, digest(21)));
    let index = TestIndex {
        statements,
        bindings: HashMap::from([(digest(20), (AuthorityDoor::Ground, digest(21)))]),
        ..Default::default()
    };

    let manifest = assemble_manifest(vec![instruction(digest(20))], &index, &real_provider()).unwrap();

    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn a_signature_bound_to_another_request_does_not_ground_the_slot() {
    // The other half of the binding tripwire: a genuine owner signature, the
    // real provider, the door the index records — and a binding that is not the
    // one recorded. Only the binding differs from the admitting case above, so
    // this refusal isolates it.
    let mut statements = HashMap::new();
    statements.insert(digest(22), signed_at(AuthorityDoor::Ground, digest(23)));
    let index = TestIndex { statements, ..Default::default() };

    let violation = assemble_manifest(vec![instruction(digest(22))], &index, &real_provider()).unwrap_err();

    assert_eq!(violation, ClosureViolation::UnverifiedSignature { slot: digest(22) });
}

#[test]
fn a_request_door_record_cannot_ground_an_instruction_slot() {
    // ADR-0182: `Ground` exists so the closure walk cannot borrow a request
    // door's envelope. Everything here is genuine — the owner really signed
    // this, at the answer door, bound to exactly what the index records — and it
    // is still refused, because grounding on it would spend a signature that
    // authorized a mutation a second time as instruction provenance. A walk that
    // verified under whatever door the index handed back would admit this.
    let mut statements = HashMap::new();
    statements.insert(digest(24), signed_at(AuthorityDoor::Answer, digest(24)));
    let index = TestIndex {
        statements,
        bindings: HashMap::from([(digest(24), (AuthorityDoor::Answer, digest(24)))]),
        ..Default::default()
    };

    let violation = assemble_manifest(vec![instruction(digest(24))], &index, &real_provider()).unwrap_err();

    assert_eq!(violation, ClosureViolation::UnverifiedSignature { slot: digest(24) });
}

#[test]
fn ungrounded_context_slot_is_admissible() {
    // The closure gates only instruction slots; an ungrounded context slot is
    // fine — it is material, not command.
    let index = TestIndex { statements: HashMap::new(), policies: HashSet::new(), ..Default::default() };
    let context = Slot { artifact: digest(6), role: SlotRole::Context, parent_closure: vec![] };

    let manifest = assemble_manifest(vec![context], &index, &real_provider()).unwrap();
    assert_eq!(manifest.slots.len(), 1);
}

#[test]
fn derived_instruction_chain_is_admissible_and_authors_its_closure() {
    // A multi-hop instruction artifact (digest(10)) derives through digest(11)
    // to a signed statement at digest(12): the walk follows `parents` to the
    // ground and admits, and rewrites the slot's parent_closure to the ancestors
    // it actually traced — not whatever the caller declared.
    let mut statements = HashMap::new();
    statements.insert(digest(12), signed_statement(digest(12)));
    let mut parents = HashMap::new();
    parents.insert(digest(10), vec![digest(11)]);
    parents.insert(digest(11), vec![digest(12)]);
    let index = TestIndex { statements, parents, ..Default::default() };

    // The caller-declared closure is deliberately bogus; the assembler ignores it.
    let slot = Slot { artifact: digest(10), role: SlotRole::Instruction, parent_closure: vec![digest(99)] };
    let manifest = assemble_manifest(vec![slot], &index, &real_provider()).unwrap();

    assert_eq!(manifest.slots.len(), 1);
    assert_eq!(manifest.slots[0].parent_closure, vec![digest(11), digest(12)]);
}

#[test]
fn a_redispatched_attempt_grounds_on_the_answer_and_its_closure_names_the_question() {
    // ADR-0151: a re-dispatched held stage's prompt derives from both the adopted
    // answer (a signed statement — the instruction ground) and the parked question
    // (context). Assembly grounds the instruction slot on the answer and authors a
    // closure naming both digests, so the audit trail shows why the retry diverged
    // from its predecessor — the "manifest names both question and answer" property
    // falling out of the existing fail-closed machinery, no new type.
    let answer = digest(20);
    let question = digest(21);
    let prompt = digest(22);
    let mut statements = HashMap::new();
    statements.insert(answer, signed_statement(answer));
    let mut parents = HashMap::new();
    parents.insert(prompt, vec![answer, question]);
    let index = TestIndex { statements, parents, ..Default::default() };

    let manifest = assemble_manifest(vec![instruction(prompt)], &index, &real_provider()).unwrap();
    let closure = &manifest.slots[0].parent_closure;
    assert!(closure.contains(&answer), "the closure names the answer it grounded on");
    assert!(closure.contains(&question), "and the question, so the audit shows why the retry diverged");
}

proptest! {
    // The caller-declared parent_closure is never trusted for grounding: no
    // matter what digests it forges — including ones the index records as
    // genuine signed statements — the slot stays ungrounded, because grounding
    // follows `index.parents` (which records no edge from the artifact) and
    // never the slot field. Property over the artifact seed and the forged
    // closure. Tripwire on trusting the caller-declared closure.
    #[test]
    fn forged_parent_closure_never_grounds(
        artifact_seed in 0u8..=255u8,
        forged in prop::collection::vec(0u8..=255u8, 0..8),
    ) {
        // The artifact is not itself a recorded ground; every forged digest is,
        // so the only thing between the forgery and admission is that the walk
        // does not follow the declared closure.
        prop_assume!(!forged.contains(&artifact_seed));
        let mut statements = HashMap::new();
        for &seed in &forged {
            statements.insert(digest(seed), signed_statement(digest(seed)));
        }
        let index = TestIndex { statements, ..Default::default() };

        let slot = Slot {
            artifact: digest(artifact_seed),
            role: SlotRole::Instruction,
            parent_closure: forged.iter().map(|&seed| digest(seed)).collect(),
        };
        let violation = assemble_manifest(vec![slot], &index, &real_provider()).unwrap_err();
        prop_assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(artifact_seed) });
    }

    // A recorded derivation chain that dead-ends at an index-unknown node before
    // reaching any ground is refused, whatever the chain's seeds and length — a
    // broken edge. Property over the chain of distinct seeds.
    #[test]
    fn broken_derivation_edge_always_rejected(seeds in prop::collection::vec(0u8..=255u8, 2..6)) {
        // Dedup so the chain is a simple path (a repeat would only shorten it).
        let mut chain: Vec<u8> = Vec::new();
        for seed in seeds {
            if !chain.contains(&seed) {
                chain.push(seed);
            }
        }
        prop_assume!(chain.len() >= 2);

        // Link each node to the next; the last node has no recorded parents and
        // is neither a statement nor a policy — the broken edge.
        let mut parents = HashMap::new();
        for pair in chain.windows(2) {
            parents.insert(digest(pair[0]), vec![digest(pair[1])]);
        }
        let index = TestIndex { parents, ..Default::default() };

        let violation = assemble_manifest(vec![instruction(digest(chain[0]))], &index, &real_provider()).unwrap_err();
        prop_assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(chain[0]) });
    }
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
    let violation = assemble_manifest(vec![slot], &index, &real_provider()).unwrap_err();
    assert_eq!(violation, ClosureViolation::ClosureBudgetExceeded { slot: digest_n(0) });
}

#[test]
fn high_fanout_exceeds_budget() {
    // The budget is enforced on breadth, not just depth: a single node whose
    // fan-out alone exceeds the budget is refused as its parents are inserted,
    // before the whole fan-out is admitted into the walk.
    let fanout: Vec<Digest> = (1..(MANIFEST_CLOSURE_BUDGET + 5)).map(digest_n).collect();
    let mut parents = HashMap::new();
    parents.insert(digest_n(0), fanout);
    let index = TestIndex { parents, ..Default::default() };

    let slot = Slot { artifact: digest_n(0), role: SlotRole::Instruction, parent_closure: vec![] };
    let violation = assemble_manifest(vec![slot], &index, &real_provider()).unwrap_err();
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

    let violation = assemble_manifest(vec![instruction(digest(40))], &index, &real_provider()).unwrap_err();
    assert_eq!(violation, ClosureViolation::UngroundedInstruction { slot: digest(40) });
}
