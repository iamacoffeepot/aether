//! Fail-closed admission refusals. Each named reason has its own message so a
//! caller cannot mistake one closed door for another.

use aether_bloomery::{
    Digest, Observation, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, Statement, WorkpieceId,
    digest_of,
};
use aether_data::wire::to_vec;

use super::{AdmissionRefusal, AdmitError, admit_member, workpiece_from_listed, workpieces_from_list};
use crate::commission::import::{ImportRequest, IssueSnapshot, import};
use crate::store::{CommissionBackend, ListCommissionsResult, ListedCommission, LoadCommissionResult, SqliteStore};

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
    let admitted = admit_member(digest, loaded("wp-1", &revision, vec![auto_approval(digest)])).expect("admitted");

    assert_eq!(admitted.workpiece.id.0, "wp-1");
    assert_eq!(admitted.workpiece.scope_revision, digest);
    assert_eq!(admitted.projection.declared_surface, ["docs/guide/**"]);
    assert_eq!(admitted.description, "advisory");
    assert!(admitted.projection.signed_statement.is_none(), "an auto row is not a signed statement");
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

    let error = admit_member(claimed, result).expect_err("claimed digest is not the bytes");
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
    let error = admit_member(expected, loaded("wp-1", &revision, vec![auto_approval(current)]))
        .expect_err("draft is behind the tip");
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
    let error = admit_member(digest, loaded("wp-1", &revision, Vec::new())).expect_err("no approval");
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
    };
    let error = admit_member(digest, result).expect_err("garbage");
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
    let error = admit_member(Digest::from_bytes([1; 32]), LoadCommissionResult::Missing { id: "wp-1".to_owned() })
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
    let error = admit_member(Digest::from_bytes([1; 32]), LoadCommissionResult::Err { error: "disk".to_owned() })
        .expect_err("store");
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
    };
    match admit_member(scope, result) {
        Err(AdmitError::Refused(refusal @ AdmissionRefusal::AbsentApproval { .. })) => {
            assert!(refusal.message().contains("no stored approval"), "{}", refusal.message());
        }
        other => panic!("imported unsigned commission must not admit, got {other:?}"),
    }
}
