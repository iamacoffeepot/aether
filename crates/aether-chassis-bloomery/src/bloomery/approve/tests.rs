//! Directed tests for the pre-seal approve gate (#3571).
//!
//! The gate + membership-approval forming: the ADR hard gate, completeness, the
//! tier branch, and auto versus signed-statement approval. The tier *resolver*
//! itself is `aether-bloomery`'s (#4616) and is tested beside the value there;
//! what stays here is the host's file fallback and everything the gate decides
//! on top of a resolved policy.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aether_bloomery::{
    AuthorityDoor, Ed25519KeyProvider, EvidenceKind, KeyId, Provenance, SignatureEnvelope, Statement,
    authorization_message, digest_of,
};
use aether_bloomery::{Digest, FakeKeyProvider};
use ed25519_dalek::{Signer, SigningKey};

use super::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, StatementRejected, Tier,
    approval_from_statement, load_policy, precheck_statement, verified_statement_approval,
};

/// The test tier policy the gate cases decide over: `docs/guide/**` advances on
/// its own, `crates/aether-data/**` stops at the owner, and anything else takes
/// the `judge` default.
const POLICY: &str = r#"default: judge
rules:
  - glob: "docs/guide/**"
    tier: auto
  - glob: "crates/aether-data/**"
    tier: human
"#;

fn policy() -> ApprovalPolicy {
    ApprovalPolicy::parse(POLICY).expect("test policy parses")
}

/// The repository's real seeded policy artifact.
fn seeded_policy_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../approval-policy.yml")
}

#[test]
fn the_seeded_repository_policy_parses_and_guards_itself() {
    // Tripwire: the strict parser fails closed, so a malformed edit to the real
    // policy file would refuse every admission — this test is where that failure
    // is loud. The guarded paths pin the constitutional carve-outs (including the
    // policy file's own self-listing) against an accidental edit.
    let policy = load_policy(&seeded_policy_path()).expect("seeded policy parses");
    for guarded in [
        "approval-policy.yml",
        ".github/workflows/ci.yml",
        "CLAUDE.md",
        "AGENTS.md",
        ".claude/skills/approve/SKILL.md",
        ".agents/skills/approve/x.py",
        "crates/aether-data/src/lib.rs",
    ] {
        assert_eq!(policy.resolve_surface(&[guarded.to_owned()]), Tier::Human, "{guarded} must stop at the owner");
    }
    // The submitted-and-trusted trees still advance on their own.
    assert_eq!(policy.resolve_surface(&["docs/guide/recipes/x.md".to_owned()]), Tier::Auto);
    assert_eq!(policy.resolve_surface(&["crates/aether-kit-commons/src/lib.rs".to_owned()]), Tier::Auto);
}

/// A `Completeness` with every check satisfied — the base a completeness test
/// flips one field of.
fn complete() -> Completeness {
    Completeness {
        has_problem_statement: true,
        has_design_notes: true,
        has_implementation_plan: true,
        referenced_adr_prs_merged: true,
        model_routing_count: 1,
        blocked: false,
        declared_surface_fresh: true,
        dependencies_all_closed: true,
        umbrella_integrity: true,
    }
}

/// A fixed scope-revision digest.
fn revision() -> Digest {
    Digest::from_bytes([9; 32])
}

/// A fixed projection digest — the digest of the exact facts the gate evaluated
/// (issue #3583, rider 3). Distinct from [`revision`] so a test can tell which of
/// the two parents moves an auto approval's `detail`.
fn projection() -> Digest {
    Digest::from_bytes([7; 32])
}

/// An admission request over `surface` with a complete, non-ADR, non-pre-approved
/// revision — the base a gate test varies.
fn request(surface: &[&str]) -> AdmissionRequest {
    AdmissionRequest {
        subject: revision(),
        declared_surface: surface.iter().map(|s| (*s).to_owned()).collect(),
        completeness: complete(),
        adr_touch: AdrTouch::None,
        pre_approved: false,
        projection_digest: projection(),
    }
}

fn gate_over(policy: &ApprovalPolicy) -> Gate<'_> {
    Gate::new(policy)
}

#[test]
fn each_completeness_check_fails_closed() {
    type Mutation = fn(&mut Completeness);
    let policy = policy();
    let gate = gate_over(&policy);
    // Each mutation drives the auto-tier surface, so only the completeness check
    // — never the tier — decides the refusal.
    let cases: [(Mutation, Incompleteness); 9] = [
        (|c| c.has_problem_statement = false, Incompleteness::MissingProblemStatement),
        (|c| c.has_design_notes = false, Incompleteness::MissingDesignNotes),
        (|c| c.has_implementation_plan = false, Incompleteness::MissingImplementationPlan),
        (|c| c.referenced_adr_prs_merged = false, Incompleteness::ReferencedAdrPrUnmerged),
        (|c| c.model_routing_count = 0, Incompleteness::ModelRouting(0)),
        (|c| c.blocked = true, Incompleteness::Blocked),
        (|c| c.declared_surface_fresh = false, Incompleteness::StaleDeclaredSurface),
        (|c| c.dependencies_all_closed = false, Incompleteness::OpenDependency),
        (|c| c.umbrella_integrity = false, Incompleteness::UmbrellaIntegrity),
    ];
    for (mutate, expected) in cases {
        let mut req = request(&["docs/guide/x.md"]);
        mutate(&mut req.completeness);
        assert_eq!(gate.evaluate(&req), Decision::Incomplete(expected));
    }
    // Two model routings is also a refusal (exactly-one).
    let mut req = request(&["docs/guide/x.md"]);
    req.completeness.model_routing_count = 2;
    assert_eq!(gate.evaluate(&req), Decision::Incomplete(Incompleteness::ModelRouting(2)));
}

#[test]
fn an_auto_tier_surface_forms_an_approval_bound_to_the_revision() {
    let policy = policy();
    let decision = gate_over(&policy).evaluate(&request(&["docs/guide/x.md"]));
    let Decision::AutoApproved(evidence) = decision else {
        panic!("auto tier must form an approval, got {decision:?}");
    };
    // Exactly the seal-time `validate_member_admission` check: an Approval bound
    // to its own scope revision.
    assert_eq!(evidence.kind, EvidenceKind::Approval);
    assert!(evidence.validates(&revision()), "the approval must bind the exact scope revision");
    // The detail names a distinct supporting artifact, not the revision itself.
    assert_ne!(evidence.detail, revision());
}

#[test]
fn the_auto_approval_detail_binds_the_evaluated_projection() {
    // Rider 3 (#3583): the auto approval's `detail` folds in the digest of the
    // exact projection facts the gate evaluated, so the sealed evidence attests
    // which projection produced the grant. A computed-value tripwire: swapping
    // only the projection digest (same surface, same revision) must move the
    // `detail`, or the binding is not actually threaded through.
    let policy = policy();
    let gate = gate_over(&policy);
    let base = request(&["docs/guide/x.md"]);
    let mut swapped = request(&["docs/guide/x.md"]);
    swapped.projection_digest = Digest::from_bytes([3; 32]);
    let (Decision::AutoApproved(a), Decision::AutoApproved(b)) = (gate.evaluate(&base), gate.evaluate(&swapped)) else {
        panic!("both requests resolve auto");
    };
    // Same subject (the shared revision), different detail (the projection moved).
    assert_eq!(a.subject, b.subject, "both approvals still bind the same scope revision");
    assert_ne!(a.detail, b.detail, "a different evaluated projection must move the auto approval's detail");
}

#[test]
fn an_above_auto_surface_requires_a_signed_statement() {
    let policy = policy();
    let gate = gate_over(&policy);
    // A human-tier surface and a judge-default surface both defer to a statement.
    assert_eq!(gate.evaluate(&request(&["crates/aether-data/src/lib.rs"])), Decision::RequiresStatement(Tier::Human));
    assert_eq!(gate.evaluate(&request(&["unknown-top/x.rs"])), Decision::RequiresStatement(Tier::Judge));
}

#[test]
fn a_new_or_established_adr_touch_routes_to_the_owner_regardless_of_policy() {
    let policy = policy();
    let gate = gate_over(&policy);
    // Even an auto-tier surface and a pre-approval override cannot pass a firing
    // ADR hard gate.
    let mut req = request(&["docs/guide/x.md"]);
    req.adr_touch = AdrTouch::NewOrEstablished;
    req.pre_approved = true;
    assert_eq!(gate.evaluate(&req), Decision::RequiresStatement(Tier::Human));
}

#[test]
fn a_proposed_only_adr_touch_defers_to_the_policy() {
    let policy = policy();
    let mut req = request(&["docs/guide/x.md"]);
    req.adr_touch = AdrTouch::ProposedOnly;
    // A still-Proposed touch is not the owner's; the auto surface advances.
    assert!(matches!(gate_over(&policy).evaluate(&req), Decision::AutoApproved(_)));
}

#[test]
fn pre_approval_waives_the_tier_but_not_the_gate_checks() {
    let policy = policy();
    let gate = gate_over(&policy);
    // A human-tier surface with the owner override resolves auto and forms the
    // approval directly.
    let mut req = request(&["crates/aether-data/src/lib.rs"]);
    req.pre_approved = true;
    assert!(matches!(gate.evaluate(&req), Decision::AutoApproved(_)), "the override waives the tier");
    // But it does not waive a completeness check.
    let mut blocked = request(&["crates/aether-data/src/lib.rs"]);
    blocked.pre_approved = true;
    blocked.completeness.blocked = true;
    assert_eq!(gate.evaluate(&blocked), Decision::Incomplete(Incompleteness::Blocked));
}

/// A deterministic signing key from a fixed seed (no rng, reproducible) — mirrors
/// the signing capability's own test helper.
fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// A provider trusting exactly `signer` at `key`'s public half.
fn provider(signer: &str, key: &SigningKey) -> Ed25519KeyProvider {
    Ed25519KeyProvider::new(BTreeMap::from([(KeyId(signer.to_owned()), key.verifying_key())]))
}

/// An author-signed statement over `words` by `signer` using `key`, signed as
/// approve-door authority bound to `bound` (ADR-0182) — the message
/// `approval_from_statement` verifies against.
fn signed_statement(signer: &str, key: &SigningKey, words: &[u8], bound: Digest) -> Statement {
    let message = authorization_message(AuthorityDoor::Approve, bound, words);
    Statement {
        words: words.to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId(signer.to_owned()),
            signature: key.sign(message.as_bytes()).to_bytes().to_vec(),
        }),
        parents: vec![],
    }
}

#[test]
fn an_authorized_signed_statement_over_the_revision_forms_the_approval() {
    let key = signing_key(7);
    let keys = provider("owner", &key);
    let statement = signed_statement("owner", &key, revision().as_bytes(), revision());
    let evidence = approval_from_statement(revision(), &statement, &keys).expect("verified statement forms approval");
    assert_eq!(evidence.kind, EvidenceKind::Approval);
    assert!(evidence.validates(&revision()));
    assert_eq!(evidence.detail, digest_of(&statement), "the detail names the signed statement");
}

#[test]
fn a_statement_over_another_revision_is_rejected() {
    let key = signing_key(7);
    let keys = provider("owner", &key);
    // Signed correctly, but over a different revision's bytes — never approves
    // this one (old evidence never validates a replacement).
    let other = Digest::from_bytes([1; 32]);
    let statement = signed_statement("owner", &key, other.as_bytes(), other);
    assert_eq!(approval_from_statement(revision(), &statement, &keys), Err(StatementRejected::WrongSubject));
}

#[test]
fn a_non_author_statement_is_rejected() {
    let keys = FakeKeyProvider;
    // Words bind the revision, but the provenance carries no author signature.
    let statement = Statement {
        words: revision().as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(aether_bloomery::Observation { source: "adapter".to_owned() }),
        parents: vec![],
    };
    assert_eq!(approval_from_statement(revision(), &statement, &keys), Err(StatementRejected::NotAnAuthorSignature));
}

#[test]
fn a_signature_outside_the_key_policy_is_rejected() {
    let key = signing_key(7);
    // A genuine signature over the right revision, but the signer is not in the
    // allowlist — fail-closed, distinct from the tier policy (who may sign).
    let keys = provider("owner", &key);
    let statement = signed_statement("intruder", &key, revision().as_bytes(), revision());
    assert_eq!(approval_from_statement(revision(), &statement, &keys), Err(StatementRejected::Unverified));
}

#[test]
fn a_signature_bound_to_another_revision_is_rejected_even_with_the_right_words() {
    // ADR-0182: the words and the signed binding are separate checks, and this
    // door keeps both. The words are exactly this revision's bytes, so the
    // synchronous precheck passes; only the signed binding names another
    // revision, so verification is what refuses. Tripwire: were the binding
    // dropped back out of the signed message, this statement would approve a
    // revision it was never signed for.
    let key = signing_key(7);
    let keys = provider("owner", &key);
    let other = Digest::from_bytes([1; 32]);
    let statement = signed_statement("owner", &key, revision().as_bytes(), other);

    assert_eq!(precheck_statement(revision(), &statement), Ok(()), "the words bind this revision, so precheck passes");
    assert_eq!(approval_from_statement(revision(), &statement, &keys), Err(StatementRejected::Unverified));
}

#[test]
fn precheck_rejects_a_wrong_subject_and_a_non_author_statement_without_a_key_policy() {
    let key = signing_key(7);
    // A genuine author signature, but over another revision's bytes — the
    // synchronous pre-check refuses it before any signature verification.
    let other = Digest::from_bytes([1; 32]);
    let wrong_subject = signed_statement("owner", &key, other.as_bytes(), other);
    assert_eq!(precheck_statement(revision(), &wrong_subject), Err(StatementRejected::WrongSubject));

    // The right subject, but no author signature — never instruction-capable.
    let non_author = Statement {
        words: revision().as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(aether_bloomery::Observation { source: "adapter".to_owned() }),
        parents: vec![],
    };
    assert_eq!(precheck_statement(revision(), &non_author), Err(StatementRejected::NotAnAuthorSignature));

    // A correct-subject author signature passes the pre-check regardless of
    // whether the signature itself verifies — that check is the caller's next step.
    let ok = signed_statement("owner", &key, revision().as_bytes(), revision());
    assert_eq!(precheck_statement(revision(), &ok), Ok(()));
}

#[test]
fn verified_statement_approval_binds_the_revision_and_details_the_statement() {
    let key = signing_key(7);
    let statement = signed_statement("owner", &key, revision().as_bytes(), revision());
    let evidence = verified_statement_approval(revision(), &statement);
    assert_eq!(evidence.kind, EvidenceKind::Approval);
    assert!(evidence.validates(&revision()), "the formed approval binds the revision");
    assert_eq!(evidence.detail, digest_of(&statement), "the detail names the signed statement");
    // The split helper forms the exact evidence the composed reader returns on a
    // verified statement — the deferred-verify seal path reuses this format.
    let keys = provider("owner", &key);
    assert_eq!(approval_from_statement(revision(), &statement, &keys), Ok(evidence));
}
