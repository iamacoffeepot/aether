//! Directed tests for the pre-seal approve gate (#3571).
//!
//! Two surfaces: the ported tier resolver (the security core — its fail-closed
//! semantics must be exact, mirroring `scripts/test-surface-match.py`'s `--tier`
//! cases), and the gate + membership-approval forming (ADR hard gate,
//! completeness, tier branch, auto vs signed-statement approval).

use std::collections::BTreeMap;
use std::path::PathBuf;

use aether_bloomery::{Digest, FakeKeyProvider};
use aether_bloomery::{Ed25519KeyProvider, EvidenceKind, KeyId, Provenance, SignatureEnvelope, Statement, digest_of};
use ed25519_dalek::{Signer, SigningKey};

use super::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, StatementRejected, Tier,
    approval_from_statement, precheck_statement, verified_statement_approval,
};

/// The test tier policy — the same shape `test-surface-match.py`'s `POLICY` uses,
/// so the ported resolver is checked against the same cases.
const POLICY: &str = r#"default: judge
rules:
  - glob: "/Cargo.toml"
    tier: human
  - glob: "crates/*/Cargo.toml"
    tier: human
  - glob: "crates/aether-data/**"
    tier: human
  - glob: "docs/adr/**"
    tier: human
  - glob: ".agents/**"
    tier: human
  - glob: "docs/guide/**"
    tier: auto
  - glob: "crates/aether-kit/**"
    tier: auto
  - glob: "crates/aether-substrate-bundle/**"
    tier: judge
  - glob: "scripts/surface-match.py"
    tier: human
"#;

fn policy() -> ApprovalPolicy {
    ApprovalPolicy::parse(POLICY).expect("test policy parses")
}

/// Resolve one surface glob to its tier through the public surface reducer.
fn tier(surface: &str) -> Tier {
    policy().resolve_surface(&[surface.to_owned()])
}

#[test]
fn exact_paths_resolve_their_rule_tier() {
    assert_eq!(tier("docs/guide/page.md"), Tier::Auto);
    assert_eq!(tier("Cargo.toml"), Tier::Human);
    assert_eq!(tier("new-top/file.txt"), Tier::Judge);
}

#[test]
fn an_exact_path_respects_directory_tail_semantics() {
    // A bare `crates/aether-kit` is set-sound: `crates/*/Cargo.toml` (human) can
    // match `crates/aether-kit/Cargo.toml` beneath it, so the tier is human even
    // though `crates/aether-kit/**` is auto.
    assert_eq!(tier("crates/aether-kit"), Tier::Human);
}

#[test]
fn literal_subtrees_are_set_sound() {
    let cases = [
        ("docs/guide/**", Tier::Auto),
        ("crates/aether-kit/src/**", Tier::Auto),
        ("crates/aether-kit/**", Tier::Human),
        ("docs/**", Tier::Human),
        ("crates/aether-substrate-bundle/new/**", Tier::Judge),
        ("new-top/**", Tier::Judge),
    ];
    for (surface, expected) in cases {
        assert_eq!(tier(surface), expected, "{surface}");
    }
}

#[test]
fn complex_surface_wildcards_fail_closed() {
    for surface in ["**", "docs/*", "crates/aether-*/future/**", "docs/[ag]uide/**"] {
        assert_eq!(tier(surface), Tier::Human, "{surface} must fail closed to human");
    }
}

#[test]
fn out_of_grammar_surface_resolves_human() {
    for surface in ["docs/guide/../adr/0001-x.md", "/docs/guide/page.md", "docs//guide/page.md"] {
        assert_eq!(tier(surface), Tier::Human, "{surface}");
    }
}

#[test]
fn a_surface_past_the_segment_cap_folds_to_human_not_deep_recursion() {
    // Tripwire: a declared surface deeper than the grammar's segment cap drives
    // the intersects matcher, whose recursion is bounded by segment count; the
    // cap must refuse it at the grammar boundary (→ Human, fail-closed) rather
    // than recurse per path segment. A 4000-segment path would overflow the
    // stack if the cap were removed.
    let deep = vec!["a"; 4000].join("/");
    assert_eq!(tier(&deep), Tier::Human, "an over-cap surface must fold to human");

    // The same ceiling gates the policy side: an over-cap policy glob fails the
    // parse closed rather than becoming a rule the matcher recurses over.
    let deep_rule = vec!["a"; 4000].join("/");
    let policy_text = format!("default: judge\nrules:\n  - glob: {deep_rule}\n    tier: auto\n");
    assert!(ApprovalPolicy::parse(&policy_text).is_none(), "an over-cap policy glob must fail the parse");
}

#[test]
fn most_restrictive_across_the_declared_surface_wins() {
    let surface = vec!["docs/guide/**".to_owned(), "crates/aether-data/src/lib.rs".to_owned()];
    assert_eq!(policy().resolve_surface(&surface), Tier::Human);
}

#[test]
fn an_empty_surface_resolves_the_policy_default() {
    assert_eq!(policy().resolve_surface(&[]), Tier::Judge);
}

#[test]
fn malformed_policy_fails_closed() {
    let malformed = [
        "default: judge\nrules:\n  - glob: \"docs/**\"\n    tier: owner\n",
        "default: judge\nrules:\n  - glob: \"docs//**\"\n    tier: auto\n",
        "default: judge\nrules:\n  - glob: \"docs***\"\n    tier: auto\n",
        "default: judge\nrules:\n  - glob: \"../**\"\n    tier: auto\n",
        "default: judge\nrules:\n- glob: docs/guide/**\ntier: auto\n",
        "default: judge\nrules:\n  - glob: docs/guide/**\n  tier: auto\n",
        "default: judge\ndefault: auto\nrules:\n  - glob: docs/**\n    tier: auto\n",
        "rules:\n  - glob: docs/**\n    tier: auto\n",
        "default: judge\nrules:\n",
        "",
    ];
    for text in malformed {
        assert!(ApprovalPolicy::parse(text).is_none(), "must fail closed: {text:?}");
    }
}

#[test]
fn well_formed_policy_with_single_star_and_default_parses() {
    let policy = policy();
    // The `crates/*/Cargo.toml` single-star segment resolves a nested manifest to
    // human, and an unmatched top-level surface takes the judge default.
    assert_eq!(policy.resolve_surface(&["crates/aether-behavior/Cargo.toml".to_owned()]), Tier::Human);
    assert_eq!(policy.resolve_surface(&["unknown-top/thing.rs".to_owned()]), Tier::Judge);
}

/// The repository's real seeded policy artifact.
fn seeded_policy_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bloomery/approval-policy.yml")
}

#[test]
fn the_seeded_repository_policy_parses_and_guards_itself() {
    // Tripwire: the strict parser fails closed, so a malformed edit to the real
    // policy file would refuse every admission — this test is where that failure
    // is loud. The guarded paths pin the constitutional carve-outs (including the
    // policy file's own self-listing) against an accidental edit.
    let policy = ApprovalPolicy::load(&seeded_policy_path()).expect("seeded policy parses");
    for guarded in [
        "bloomery/approval-policy.yml",
        ".github/approval-policy.yml",
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
    assert_eq!(policy.resolve_surface(&["crates/aether-kit/src/lib.rs".to_owned()]), Tier::Auto);
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
        scope_revision: revision(),
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

/// An author-signed statement over `words` by `signer` using `key`.
fn signed_statement(signer: &str, key: &SigningKey, words: &[u8]) -> Statement {
    Statement {
        words: words.to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId(signer.to_owned()),
            signature: key.sign(words).to_bytes().to_vec(),
        }),
        parents: vec![],
    }
}

#[test]
fn an_authorized_signed_statement_over_the_revision_forms_the_approval() {
    let key = signing_key(7);
    let keys = provider("owner", &key);
    let statement = signed_statement("owner", &key, revision().as_bytes());
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
    let statement = signed_statement("owner", &key, other.as_bytes());
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
    let statement = signed_statement("intruder", &key, revision().as_bytes());
    assert_eq!(approval_from_statement(revision(), &statement, &keys), Err(StatementRejected::Unverified));
}

#[test]
fn precheck_rejects_a_wrong_subject_and_a_non_author_statement_without_a_key_policy() {
    let key = signing_key(7);
    // A genuine author signature, but over another revision's bytes — the
    // synchronous pre-check refuses it before any signature verification.
    let other = Digest::from_bytes([1; 32]);
    let wrong_subject = signed_statement("owner", &key, other.as_bytes());
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
    let ok = signed_statement("owner", &key, revision().as_bytes());
    assert_eq!(precheck_statement(revision(), &ok), Ok(()));
}

#[test]
fn verified_statement_approval_binds_the_revision_and_details_the_statement() {
    let key = signing_key(7);
    let statement = signed_statement("owner", &key, revision().as_bytes());
    let evidence = verified_statement_approval(revision(), &statement);
    assert_eq!(evidence.kind, EvidenceKind::Approval);
    assert!(evidence.validates(&revision()), "the formed approval binds the revision");
    assert_eq!(evidence.detail, digest_of(&statement), "the detail names the signed statement");
    // The split helper forms the exact evidence the composed reader returns on a
    // verified statement — the deferred-verify seal path reuses this format.
    let keys = provider("owner", &key);
    assert_eq!(approval_from_statement(revision(), &statement, &keys), Ok(evidence));
}
