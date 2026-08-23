//! Fail-closed admission refusals. Each named reason has its own message so a
//! caller cannot mistake one closed door for another.

use aether_bloomery::{
    CommissionStatus, Digest, MemberDependency, Observation, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision,
    ScopeRouting, Statement, WorkpieceId, digest_of,
};
use aether_data::wire::to_vec;

use super::adr_touch::{AbsentAdrs, AdrMaturity, SealedAdrStatus};
use super::{
    AdmissionRefusal, AdmitError, AdmittedMember, DependencyResolution, admit_member, workpiece_from_listed,
    workpieces_from_list,
};
use crate::bloomery::{AdmissionRequest, AdrTouch, ApprovalPolicy, Decision, Gate, Tier};
use crate::commission::import::{ImportRequest, IssueSnapshot, import};
use crate::store::{CommissionBackend, ListCommissionsResult, ListedCommission, LoadCommissionResult, SqliteStore};

fn admit(expected: Digest, result: LoadCommissionResult) -> Result<AdmittedMember, AdmitError> {
    admit_member(expected, result, &AbsentAdrs, &DependencyResolution::default())
}

fn revision(id: &str, problem: &str) -> ScopeRevision {
    ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: WorkpieceId(id.to_owned()),
        predecessor: None,
        problem: problem.to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: vec!["docs/guide/**".to_owned()],
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
        dependencies: Vec::new(),
        description: "advisory".to_owned(),
        implements: Vec::new(),
        declared_crates: Vec::new(),
        declared_reads: Vec::new(),
    }
}

fn auto_approval(scope: Digest) -> Vec<u8> {
    to_vec(&Statement {
        words: scope.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "test".to_owned() }),
        parents: vec![scope],
    })
    .expect("statement encodes")
}

fn loaded(id: &str, revision: &ScopeRevision, approvals: Vec<Vec<u8>>) -> LoadCommissionResult {
    let digest = digest_of(revision);
    LoadCommissionResult::Ok {
        id: id.to_owned(),
        intent: vec![2; 32],
        current_revision: Some(digest.as_bytes().to_vec()),
        current_ordinal: Some(1),
        status: "open".to_owned(),
        current: Some(revision.to_canonical()),
        approvals,
        scope_verify: None,
    }
}

fn listed(id: &str, revision: Option<Digest>) -> ListedCommission {
    ListedCommission {
        id: id.to_owned(),
        intent: vec![2; 32],
        current_revision: revision.map(|digest| digest.as_bytes().to_vec()),
        current_ordinal: revision.map(|_| 1),
        status: "open".to_owned(),
    }
}

#[test]
fn a_verified_row_materializes_the_workpiece_and_the_frozen_projection() {
    // The door reads the canonical revision, not a caller projection of the
    // same digest. A swap of surface or description at the request would
    // otherwise admit work the operator did not sign.
    let revision = revision("wp-1", "problem");
    let digest = digest_of(&revision);
    let admitted = admit(digest, loaded("wp-1", &revision, vec![auto_approval(digest)])).expect("admitted");

    assert_eq!(admitted.workpiece.id.0, "wp-1");
    assert_eq!(admitted.workpiece.scope_revision, digest);
    assert_eq!(admitted.projection.declared_surface, ["docs/guide/**"]);
    // The advisory body is the revision's own, and the surface block under it
    // is rendered from `declared_surface` rather than trusted from the text: an
    // amendment widens the field and carries the body forward unchanged, so the
    // field is what the work order has to state.
    assert_eq!(admitted.description, "advisory\n\n## Declared surface\n\ndocs/guide/**\n");
    assert!(admitted.projection.signed_statement.is_none(), "an auto row is not a signed statement");
}

#[test]
fn an_empty_description_is_refused_not_dispatched() {
    // Tripwire: an empty description used to miss the door and dispatch an
    // empty ## Task. Structured fields are not a substitute once the verb
    // stores the rendered work order on the revision.
    let mut revision = revision("wp-local", "Need a CLI.");
    revision.description.clear();
    let digest = digest_of(&revision);
    let error = admit(digest, loaded("wp-local", &revision, vec![auto_approval(digest)]))
        .expect_err("empty description must not admit");
    match error {
        AdmitError::Refused(refusal @ AdmissionRefusal::EmptyDescription { .. }) => {
            assert!(refusal.message().contains("empty description"), "{}", refusal.message());
        }
        other => panic!("expected empty description, got {other:?}"),
    }
}

#[test]
fn a_draft_naming_a_cancelled_commission_is_not_stale() {
    // Tripwire: a cancelled tip used to fail as StaleScope, sending the
    // operator to write a new revision instead of naming the closed status.
    let revision = revision("wp-1", "problem");
    let digest = digest_of(&revision);
    let mut result = loaded("wp-1", &revision, vec![auto_approval(digest)]);
    if let LoadCommissionResult::Ok { status, .. } = &mut result {
        *status = "cancelled".to_owned();
    }
    let error = admit(digest, result).expect_err("cancelled commission must not admit");
    match error {
        AdmitError::Refused(refusal @ AdmissionRefusal::NotOpen { .. }) => {
            assert!(refusal.message().contains("not open"), "{}", refusal.message());
            assert!(!refusal.message().contains("stale"), "status must not read as stale: {}", refusal.message());
        }
        other => panic!("expected not open, got {other:?}"),
    }
}

#[test]
fn digest_mismatch_is_not_a_stale_scope() {
    // The index column names digest A but the canonical bytes hash to B.
    // Treating that as stale would hide corruption behind a "write a new
    // revision" retry that cannot fix the row.
    let revision = revision("wp-1", "problem");
    let claimed = Digest::from_bytes([9; 32]);
    let mut result = loaded("wp-1", &revision, vec![auto_approval(digest_of(&revision))]);
    if let LoadCommissionResult::Ok { current_revision, .. } = &mut result {
        *current_revision = Some(claimed.as_bytes().to_vec());
    }

    let error = admit(claimed, result).expect_err("claimed digest is not the bytes");
    match error {
        AdmitError::Refused(refusal @ AdmissionRefusal::DigestMismatch { .. }) => {
            assert!(refusal.message().contains("does not match its canonical bytes"), "{}", refusal.message());
            assert!(!refusal.message().contains("stale"), "mismatch must not read as stale: {}", refusal.message());
        }
        other => panic!("expected digest mismatch, got {other:?}"),
    }
}

#[test]
fn a_draft_naming_a_superseded_revision_is_stale() {
    // The store tip moved. The draft still names the revision the operator
    // approved earlier — that is stale, not a digest mismatch: the bytes
    // hash correctly, they are just no longer current.
    let revision = revision("wp-1", "problem");
    let current = digest_of(&revision);
    let expected = Digest::from_bytes([3; 32]);
    let error =
        admit(expected, loaded("wp-1", &revision, vec![auto_approval(current)])).expect_err("draft is behind the tip");
    match error {
        AdmitError::Refused(refusal @ AdmissionRefusal::StaleScope { .. }) => {
            assert!(refusal.message().contains("stale scope revision"), "{}", refusal.message());
            assert!(
                !refusal.message().contains("canonical bytes"),
                "stale must not read as mismatch: {}",
                refusal.message()
            );
        }
        other => panic!("expected stale scope, got {other:?}"),
    }
}

#[test]
fn a_current_revision_with_no_approval_is_absent_not_malformed() {
    // An open tip with no approval row is the operator skipping submit, not
    // a decode failure. A malformed message would send them to repair bytes
    // that are fine.
    let revision = revision("wp-1", "problem");
    let digest = digest_of(&revision);
    let error = admit(digest, loaded("wp-1", &revision, Vec::new())).expect_err("no approval");
    match error {
        AdmitError::Refused(refusal @ AdmissionRefusal::AbsentApproval { .. }) => {
            assert!(refusal.message().contains("no stored approval"), "{}", refusal.message());
            assert!(
                !refusal.message().contains("malformed"),
                "absent must not read as malformed: {}",
                refusal.message()
            );
        }
        other => panic!("expected absent approval, got {other:?}"),
    }
}

#[test]
fn garbage_canonical_bytes_are_malformed() {
    // A row whose body will not decode must not become a ScopeRevision, and
    // the message must not look like a missing approval — the approval was
    // never reached.
    let digest = Digest::from_bytes([4; 32]);
    let result = LoadCommissionResult::Ok {
        id: "wp-1".to_owned(),
        intent: vec![2; 32],
        current_revision: Some(digest.as_bytes().to_vec()),
        current_ordinal: Some(1),
        status: "open".to_owned(),
        current: Some(vec![0xff, 0x00]),
        approvals: vec![auto_approval(digest)],
        scope_verify: None,
    };
    let error = admit(digest, result).expect_err("garbage");
    match error {
        AdmitError::Refused(refusal @ AdmissionRefusal::MalformedCanonical { .. }) => {
            assert!(refusal.message().contains("malformed"), "{}", refusal.message());
            assert!(
                !refusal.message().contains("no stored approval"),
                "malformed must not read as absent: {}",
                refusal.message()
            );
        }
        other => panic!("expected malformed, got {other:?}"),
    }
}

#[test]
fn a_missing_commission_is_not_an_absent_approval() {
    // No row at all is not "forgot to approve". The operator has not created
    // the commission; telling them it has no approval would send them to the
    // wrong route.
    let error = admit(Digest::from_bytes([1; 32]), LoadCommissionResult::Missing { id: "wp-1".to_owned() })
        .expect_err("missing");
    match error {
        AdmitError::Refused(refusal @ AdmissionRefusal::MissingCommission { .. }) => {
            assert!(refusal.message().contains("no commission in the store"), "{}", refusal.message());
            assert!(
                !refusal.message().contains("no stored approval"),
                "missing must not read as absent approval: {}",
                refusal.message()
            );
        }
        other => panic!("expected missing commission, got {other:?}"),
    }
}

#[test]
fn a_store_fault_is_not_an_admission_refusal() {
    // 5xx names transport; 422 names a closed door. Collapsing a disk error
    // into malformed would make a retryable fault look like bad bytes.
    let error =
        admit(Digest::from_bytes([1; 32]), LoadCommissionResult::Err { error: "disk".to_owned() }).expect_err("store");
    assert!(matches!(error, AdmitError::Store(_)), "got {error:?}");
    assert_eq!(error.response().status, 500);
}

#[test]
fn list_materializes_open_heads_and_skips_a_revisionless_commission() {
    // GET /workpieces is the durable list. A commission that has no current
    // revision is not a Workpiece — omitting it is not a silent drop of a
    // sealable member, because seal would refuse that id as stale.
    let revision = revision("wp-1", "problem");
    let digest = digest_of(&revision);
    let workpieces = workpieces_from_list(ListCommissionsResult::Ok {
        commissions: vec![listed("wp-1", Some(digest)), listed("wp-2", None)],
    })
    .expect("list");
    assert_eq!(workpieces.len(), 1);
    assert_eq!(workpieces[0].id.0, "wp-1");
    assert_eq!(workpieces[0].scope_revision, digest);
}

#[test]
fn a_cancelled_head_is_not_listed_as_a_workpiece() {
    let revision = revision("wp-1", "problem");
    let mut head = listed("wp-1", Some(digest_of(&revision)));
    head.status = "cancelled".to_owned();
    assert_eq!(workpiece_from_listed(&head).expect("decode"), None);
}

#[test]
fn an_imported_unsigned_commission_cannot_seal() {
    // Import writes observation-attested intent and a revision, and must not
    // insert an approval. If it did, admit_member would treat GitHub trust
    // as enough to seal.
    let mut store = SqliteStore::open(":memory:").expect("in-memory store opens");
    let body = "\
## Problem statement

Need a commission store.

## Design notes

Import without granting authority.

## Implementation plan

Write the offline importer.

**Size:** m
**Implementation model:** sonnet
**Routing reason:** migration rehearsal

## Declared surface

```text
crates/aether-chassis-bloomery/src/commission/import/**
```
";
    import(
        &mut store,
        &ImportRequest {
            issues: vec![IssueSnapshot {
                number: 10,
                workpiece: WorkpieceId("issue-10".to_owned()),
                title: "Need a commission store".to_owned(),
                body: body.to_owned(),
            }],
            sealed: Vec::new(),
        },
    )
    .expect("import");
    let view = store.load(&WorkpieceId("issue-10".to_owned())).expect("load").expect("exists");
    let scope = view.head.current_revision.expect("clean body writes a revision");
    let approvals = store.load_approvals(scope).expect("approvals");
    let result = LoadCommissionResult::Ok {
        id: "issue-10".to_owned(),
        intent: view.head.intent.as_bytes().to_vec(),
        current_revision: Some(scope.as_bytes().to_vec()),
        current_ordinal: view.head.current_ordinal,
        status: view.head.status.as_str().to_owned(),
        current: view.current.map(|revision| revision.to_canonical()),
        approvals: approvals.iter().map(|statement| to_vec(statement).expect("encode")).collect(),
        scope_verify: None,
    };
    match admit(scope, result) {
        Err(AdmitError::Refused(refusal @ AdmissionRefusal::AbsentApproval { .. })) => {
            assert!(refusal.message().contains("no stored approval"), "{}", refusal.message());
        }
        other => panic!("imported unsigned commission must not admit, got {other:?}"),
    }
}

/// A sealed-base catalog that answers only the listed paths.
struct Catalog<'a>(&'a [(&'a str, SealedAdrStatus)]);

impl AdrMaturity for Catalog<'_> {
    fn status(&self, path: &str) -> Option<SealedAdrStatus> {
        self.0.iter().copied().find(|(candidate, _)| *candidate == path).map(|(_, status)| status)
    }
}

fn auto_policy() -> ApprovalPolicy {
    ApprovalPolicy { default: Tier::Auto, rules: Vec::new() }
}

fn gate_request(admitted: &AdmittedMember, pre_approved: bool) -> AdmissionRequest {
    AdmissionRequest {
        subject: admitted.workpiece.scope_revision,
        declared_surface: admitted.projection.declared_surface.clone(),
        declared_crates: admitted.projection.declared_crates.clone(),
        completeness: admitted.projection.completeness,
        adr_touch: admitted.projection.adr_touch,
        pre_approved,
        projection_digest: Digest::from_bytes([7; 32]),
    }
}

#[test]
fn a_new_adr_path_routes_to_the_owner_even_when_pre_approved() {
    // Pre-fix, any docs/adr glob became ProposedOnly, so pre_approved waived the
    // tier and formed AutoApproved — the hard gate never saw NewOrEstablished.
    let mut revision = revision("wp-1", "problem");
    revision.declared_surface = vec!["docs/adr/0999-new-decision.md".to_owned()];
    let digest = digest_of(&revision);
    let admitted = admit(digest, loaded("wp-1", &revision, vec![auto_approval(digest)])).expect("admitted");

    assert_eq!(admitted.projection.adr_touch, AdrTouch::NewOrEstablished);
    assert_eq!(
        Gate::new(&auto_policy()).evaluate(&gate_request(&admitted, true)),
        Decision::RequiresStatement(Tier::Human),
    );
}

#[test]
fn a_proposed_adr_path_stays_proposed_only() {
    let path = "docs/adr/0184-calibration.md";
    let mut revision = revision("wp-1", "problem");
    revision.declared_surface = vec![path.to_owned()];
    let digest = digest_of(&revision);
    let admitted = admit_member(
        digest,
        loaded("wp-1", &revision, vec![auto_approval(digest)]),
        &Catalog(&[(path, SealedAdrStatus::Proposed)]),
        &DependencyResolution::default(),
    )
    .expect("admitted");

    assert_eq!(admitted.projection.adr_touch, AdrTouch::ProposedOnly);
    let decision = Gate::new(&auto_policy()).evaluate(&gate_request(&admitted, false));
    assert!(
        matches!(decision, Decision::AutoApproved(_)),
        "a still-Proposed touch defers to the auto policy, got {decision:?}"
    );
}

fn admit_depending(dep: &WorkpieceId, resolution: &DependencyResolution) -> AdmittedMember {
    let mut revision = revision("wp-a", "problem");
    revision.dependencies = vec![dep.clone()];
    let digest = digest_of(&revision);
    admit_member(digest, loaded("wp-a", &revision, vec![auto_approval(digest)]), &AbsentAdrs, resolution)
        .expect("member itself admits")
}

#[test]
fn a_declared_dependency_is_satisfied_only_as_a_co_sealed_member_or_a_landed_commission() {
    // Pre-fix, completeness_from wrote the literal true, so an open non-member
    // still admitted and every dependency became an ordering edge. A co-sealed
    // sibling is Open by construction; treating "not open" as satisfied would
    // refuse the primary use of declared edges.
    let dep = WorkpieceId("wp-dep".to_owned());
    let member = WorkpieceId("wp-a".to_owned());
    let none: Vec<(WorkpieceId, CommissionStatus)> = Vec::new();
    let cases = [
        ("co-sealed", DependencyResolution::new(vec![dep.clone()], none.clone()), true, true),
        (
            "landed",
            DependencyResolution::new(Vec::<WorkpieceId>::new(), vec![(dep.clone(), CommissionStatus::Landed)]),
            true,
            false,
        ),
        (
            "open non-member",
            DependencyResolution::new(Vec::<WorkpieceId>::new(), vec![(dep.clone(), CommissionStatus::Open)]),
            false,
            false,
        ),
        (
            "cancelled",
            DependencyResolution::new(Vec::<WorkpieceId>::new(), vec![(dep.clone(), CommissionStatus::Cancelled)]),
            false,
            false,
        ),
        ("missing", DependencyResolution::new(Vec::<WorkpieceId>::new(), none), false, false),
    ];
    for (name, resolution, closed, edge) in cases {
        let admitted = admit_depending(&dep, &resolution);
        assert_eq!(admitted.projection.completeness.dependencies_all_closed, closed, "{name}: completeness bit");
        let expected_edges = if edge {
            vec![MemberDependency { member: member.clone(), depends_on: dep.clone() }]
        } else {
            Vec::new()
        };
        assert_eq!(admitted.edges, expected_edges, "{name}: derived edges");
    }
}
