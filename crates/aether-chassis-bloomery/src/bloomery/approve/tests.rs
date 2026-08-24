//! Directed tests for the pre-seal approve gate (#3571).
//!
//! The gate + membership-approval forming: the ADR hard gate, completeness, the
//! tier branch, and auto versus signed-statement approval. The tier *resolver*
//! itself is `aether-bloomery`'s (#4616) and is tested beside the value there;
//! what stays here is the host's file fallback and everything the gate decides
//! on top of a resolved policy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use aether_bloomery::{
    ApprovalRule, AuthorityDoor, Ed25519KeyProvider, EvidenceKind, KeyId, Provenance, SignatureEnvelope, Statement,
    authorization_message, digest_of,
};
use aether_bloomery::{DRAFT_ADMISSION_GATE, Digest, FakeKeyProvider};
use ed25519_dalek::{Signer, SigningKey};

use super::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, StatementRejected, Tier,
    approval_from_statement, check_signer_tier, load_policy, precheck_statement, verified_statement_approval,
};

/// The test tier policy the gate cases decide over: `docs/guide/**` advances on
/// its own, `crates/aether-data/**` stops at the owner, and anything else takes
/// the `judge` default.
fn policy() -> ApprovalPolicy {
    ApprovalPolicy {
        default: Tier::Judge,
        rules: vec![
            ApprovalRule { glob: "docs/guide/**".to_owned(), tier: Tier::Auto },
            ApprovalRule { glob: "crates/aether-data/**".to_owned(), tier: Tier::Human },
        ],
    }
}

/// The repository's real seeded policy artifact.
fn seeded_policy_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../approval-policy.toml")
}

#[test]
fn the_seeded_repository_policy_parses_and_guards_itself() {
    // Tripwire: the strict parser fails closed, so a malformed edit to the real
    // policy file would refuse every admission — this test is where that failure
    // is loud. The guarded paths pin the constitutional carve-outs (including the
    // policy file's own self-listing) against an accidental edit.
    let policy = load_policy(&seeded_policy_path()).expect("seeded policy parses");
    for guarded in [
        "approval-policy.toml",
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

    // Tripwire: the two mid-segment-wildcard rules the original report
    // worried about must survive the file route. `crates/*/Cargo.toml`
    // is Human and a concrete crate manifest resolves that way;
    // `crates/aether-test-fixtures-*/**` is Auto on the loaded rule
    // (set-sound still folds an uncovered path with the Judge default,
    // so the rule's own tier is what this pins).
    assert!(
        policy.rules.iter().any(|rule| rule.glob == "crates/*/Cargo.toml" && rule.tier == Tier::Human),
        "the mid-segment crates/*/Cargo.toml rule must load as human",
    );
    assert!(
        policy.rules.iter().any(|rule| rule.glob == "crates/aether-test-fixtures-*/**" && rule.tier == Tier::Auto),
        "the mid-segment test-fixtures rule must load as auto",
    );
    assert_eq!(
        policy.resolve_surface(&["crates/x/Cargo.toml".to_owned()]),
        Tier::Human,
        "crates/*/Cargo.toml must stop at the owner",
    );
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
        declared_crates: Vec::new(),
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
    let mut guards: BTreeSet<&'static str> = BTreeSet::new();
    for (mutate, expected) in cases {
        let mut req = request(&["docs/guide/x.md"]);
        mutate(&mut req.completeness);
        let Decision::Incomplete { reason, refusal } = gate.evaluate(&req) else {
            panic!("a failed completeness check refuses");
        };
        assert_eq!(reason, expected);
        // ADR-0206: one guard per check, and the guard names the condition that
        // held rather than the failure. A refusal that named the wrong guard —
        // or the same guard for every case — would still carry the right typed
        // reason, so the guard is what this asserts.
        assert_eq!(refusal.gate, DRAFT_ADMISSION_GATE);
        assert!(!refusal.reads.is_empty(), "{} named no value it read", refusal.guard);
        assert!(guards.insert(refusal.guard), "two checks refused at the same guard: {}", refusal.guard);
    }
    assert_eq!(guards.len(), cases.len(), "every completeness check has a guard of its own");

    // Two model routings is also a refusal (exactly-one).
    let mut req = request(&["docs/guide/x.md"]);
    req.completeness.model_routing_count = 2;
    let Decision::Incomplete { reason, refusal } = gate.evaluate(&req) else {
        panic!("two model routings refuses");
    };
    assert_eq!(reason, Incompleteness::ModelRouting(2));
    assert_eq!(refusal.reads[0].value, "2", "the guard records the count it read, not just that it was wrong");
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
    assert!(
        matches!(gate.evaluate(&blocked), Decision::Incomplete { reason: Incompleteness::Blocked, .. }),
        "the override waives the tier, not a completeness check"
    );
}

/// A deterministic signing key from a fixed seed (no rng, reproducible) — mirrors
/// the signing capability's own test helper.
fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// A provider trusting exactly `signer` at `key`'s public half, authorized to
/// the top of the tier ladder.
fn provider(signer: &str, key: &SigningKey) -> Ed25519KeyProvider {
    provider_at(signer, key, Tier::Human)
}

/// A provider trusting exactly `signer` at `key`'s public half, authorized no
/// higher than `ceiling`.
fn provider_at(signer: &str, key: &SigningKey, ceiling: Tier) -> Ed25519KeyProvider {
    Ed25519KeyProvider::new(BTreeMap::from([(
        KeyId(signer.to_owned()),
        aether_bloomery::AuthorizedSigner { key: key.verifying_key(), ceiling },
    )]))
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
    let evidence =
        approval_from_statement(revision(), &statement, &keys, Tier::Human).expect("verified statement forms approval");
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
    assert_eq!(
        approval_from_statement(revision(), &statement, &keys, Tier::Human),
        Err(StatementRejected::WrongSubject)
    );
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
    assert_eq!(
        approval_from_statement(revision(), &statement, &keys, Tier::Human),
        Err(StatementRejected::NotAnAuthorSignature)
    );
}

#[test]
fn a_signature_outside_the_key_policy_is_rejected() {
    let key = signing_key(7);
    // A genuine signature over the right revision, but the signer is not in the
    // allowlist — fail-closed, distinct from the tier policy (who may sign).
    let keys = provider("owner", &key);
    let statement = signed_statement("intruder", &key, revision().as_bytes(), revision());
    assert_eq!(approval_from_statement(revision(), &statement, &keys, Tier::Human), Err(StatementRejected::Unverified));
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
    assert_eq!(approval_from_statement(revision(), &statement, &keys, Tier::Human), Err(StatementRejected::Unverified));
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
    assert_eq!(approval_from_statement(revision(), &statement, &keys, Tier::Human), Ok(evidence));
}

#[test]
fn a_signer_below_the_resolved_tier_does_not_approve() {
    // The #5324 hole, at the synchronous door: an operator key the allowlist
    // authorizes only to `judge`, signing a genuine, correctly-bound statement
    // over a surface the tier policy resolved `human`. Before the binding
    // existed this formed an approval indistinguishable from an owner's — human
    // tier was enforced by the human declining to sign, not by the machine.
    let key = signing_key(7);
    let keys = provider_at("operator", &key, Tier::Judge);
    let statement = signed_statement("operator", &key, revision().as_bytes(), revision());

    assert_eq!(
        approval_from_statement(revision(), &statement, &keys, Tier::Human),
        Err(StatementRejected::BelowTier { required: Tier::Human, ceiling: Tier::Judge }),
        "a judge-ceiling signer must not approve a human-tier surface"
    );
}

#[test]
fn a_signer_at_the_resolved_tier_approves() {
    // The same signer, the same statement, at the tier its key policy actually
    // authorizes — so the binding refuses too little rather than everything.
    let key = signing_key(7);
    let keys = provider_at("operator", &key, Tier::Judge);
    let statement = signed_statement("operator", &key, revision().as_bytes(), revision());

    assert_eq!(
        approval_from_statement(revision(), &statement, &keys, Tier::Judge)
            .expect("a judge-ceiling signer approves a judge-tier surface"),
        verified_statement_approval(revision(), &statement),
        "an in-authority signature forms the same approval it always did"
    );
}

#[test]
fn the_tier_check_refuses_a_statement_carrying_no_author_signature() {
    // Fail closed on the shape the ceiling lookup cannot answer for. Nothing
    // reaches this through `approval_from_statement` (the precheck refuses a
    // non-author statement first), so this pins the helper's own behaviour for
    // the deferred path, which composes the same three steps by hand.
    let statement = Statement {
        words: revision().as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(aether_bloomery::Observation { source: "adapter".to_owned() }),
        parents: vec![],
    };

    assert_eq!(
        check_signer_tier(&statement, &FakeKeyProvider, Tier::Auto),
        Err(StatementRejected::Unverified),
        "a statement with no signer has no ceiling, so it authorizes nothing"
    );
}
