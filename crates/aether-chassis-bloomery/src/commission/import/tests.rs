//! Rehearsal fixtures for the GitHub-to-commission import.

use std::fs;

use aether_bloomery::{
    AuthorityDoor, BloomDraft, CommissionStatus, ConfigRegistry, Digest, Evidence, EvidenceKind, Forecast, KeyId,
    Membership, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, SignatureEnvelope, Statement,
    WorkpieceId, authorization_message, digest_of,
};
use ed25519_dalek::{Signer, SigningKey};

use super::{ImportError, ImportRequest, IssueSnapshot, ParseStatus, SealedWorkpiece, Trust, import};
use crate::store::{CommissionBackend, SqliteStore};

fn memory() -> SqliteStore {
    SqliteStore::open(":memory:").expect("in-memory store opens")
}

fn workpiece(id: &str) -> WorkpieceId {
    WorkpieceId(id.to_owned())
}

fn clean_body() -> String {
    "\
<!-- aether-approval:v2 {\"authority\":\"owner\",\"base_sha\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"effective_tier\":\"human\",\"issue\":10,\"model\":\"sonnet\",\"plan_sha256\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"policy_tier\":\"human\",\"size\":\"m\"} -->

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

## Dogfood brief

Import a clean issue, then sign.
"
    .to_owned()
}

fn revision(id: &str) -> ScopeRevision {
    ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: workpiece(id),
        predecessor: None,
        problem: "problem".to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: vec!["crates/aether-bloomery/**".to_owned()],
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
        dependencies: Vec::new(),
        description: String::new(),
        implements: Vec::new(),
    }
}

fn signed_approval(scope: Digest) -> Statement {
    let key = SigningKey::from_bytes(&[7; 32]);
    let message = authorization_message(AuthorityDoor::Approve, scope, scope.as_bytes());
    Statement {
        words: scope.as_bytes().to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".to_owned()),
            signature: key.sign(message.as_bytes()).to_bytes().to_vec(),
        }),
        parents: Vec::new(),
    }
}

fn sealed_member(id: &str) -> (BloomDraft, ScopeRevision, Statement) {
    let revision = revision(id);
    let scope = digest_of(&revision);
    let approval = signed_approval(scope);
    let mut member = Membership {
        workpiece: workpiece(id),
        scope_revision: scope,
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: digest_of(&approval) },
    };
    member.approval.subject = member.subject();
    let draft = BloomDraft {
        proposals: vec![member],
        base: Digest::from_bytes([7; 32]),
        configs: ConfigRegistry::default(),
        forecast: Forecast::default(),
    };
    (draft, revision, approval)
}

#[test]
fn a_clean_issue_imports_a_revision_and_no_approval() {
    // A hidden GitHub marker is provenance, not a signature. Writing it as an
    // auto-tier approval would let the gate seal work nobody signed.
    let mut store = memory();
    let report = import(
        &mut store,
        &ImportRequest {
            issues: vec![IssueSnapshot { number: 10, workpiece: workpiece("issue-10"), body: clean_body() }],
            sealed: Vec::new(),
        },
    )
    .expect("clean import");

    assert_eq!(report.entries.len(), 1);
    assert!(matches!(report.entries[0].parse, ParseStatus::Clean { .. }));
    assert!(matches!(report.entries[0].trust, Trust::GithubObservation { .. }));
    assert_eq!(report.entries[0].base_commit.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));

    let view = store.load(&workpiece("issue-10")).expect("load").expect("exists");
    assert!(view.current.is_some(), "a clean body must become a revision");
    let scope = view.head.current_revision.expect("tip");
    assert!(store.load_approvals(scope).expect("approvals").is_empty(), "GitHub trust must not become an approval row");
    match view.current.expect("revision").workpiece.0.as_str() {
        "issue-10" => {}
        other => panic!("imported revision must keep the named workpiece, got {other}"),
    }
}

#[test]
fn an_ambiguous_issue_imports_without_a_revision() {
    // A body that does not parse cannot become a signed scope. Inventing a
    // default revision would make it approvable.
    let mut store = memory();
    let report = import(
        &mut store,
        &ImportRequest {
            issues: vec![IssueSnapshot {
                number: 11,
                workpiece: workpiece("issue-11"),
                body: "just a title and some prose\n".to_owned(),
            }],
            sealed: Vec::new(),
        },
    )
    .expect("ambiguous import");

    assert!(matches!(report.entries[0].parse, ParseStatus::Ambiguous { .. }));
    let view = store.load(&workpiece("issue-11")).expect("load").expect("exists");
    assert!(view.current.is_none(), "an unparseable body must not grow a revision");
    assert_eq!(view.head.status, CommissionStatus::Open);
}

#[test]
fn a_sealed_bloom_reconstruction_matches_the_pinned_digests() {
    // Reconstructing from a sealed membership must write the exact pinned
    // revision and evidence, and must not touch the spec bytes.
    let mut store = memory();
    let (draft, revision, approval) = sealed_member("issue-12");
    let spec = draft.seal();
    let before = spec.clone();
    let pinned_scope = spec.members()[0].scope_revision;
    let pinned_evidence = spec.members()[0].approval.detail;

    import(
        &mut store,
        &ImportRequest {
            issues: Vec::new(),
            sealed: vec![SealedWorkpiece { spec: spec.clone(), revision: revision.clone(), approval }],
        },
    )
    .expect("reconstruct");

    assert_eq!(spec, before, "import must not rewrite a sealed BloomSpec");
    assert_eq!(digest_of(&spec), digest_of(&before), "BloomSpec identity must be unchanged");
    let loaded = store.load_revision(pinned_scope).expect("load").expect("row");
    assert_eq!(loaded, revision);
    assert_eq!(digest_of(&loaded), pinned_scope);
    let approvals = store.load_approvals(pinned_scope).expect("approvals");
    assert_eq!(approvals.len(), 1);
    assert_eq!(digest_of(&approvals[0]), pinned_evidence);
}

#[test]
fn an_empty_set_is_refused_rather_than_sweeping() {
    // An implicit sweep of every snapshot on disk is the class this command
    // exists to prevent. An empty request must be a no-op refusal.
    let mut store = memory();
    match import(&mut store, &ImportRequest::default()) {
        Err(ImportError::EmptySet) => {}
        other => panic!("empty set must refuse, got {other:?}"),
    }
    assert!(store.list(None).expect("list").is_empty());
}

#[test]
fn import_paths_reads_only_the_named_bodies() {
    // A sibling file in the snapshot directory is not a workpiece. Resolving
    // the manifest as a directory listing would import it.
    let dir = tempfile::tempdir().expect("temp dir");
    let named = dir.path().join("10.md");
    let extra = dir.path().join("999.md");
    let manifest = dir.path().join("manifest.json");
    let store_path = dir.path().join("journal.sqlite");
    fs::write(&named, clean_body()).expect("write named body");
    fs::write(&extra, "not in the explicit set\n").expect("write extra body");
    fs::write(&manifest, r#"{"issues":[{"number":10,"id":"issue-10","body":"10.md"}]}"#).expect("write manifest");

    let report = super::import_paths(&manifest, &store_path, None).expect("import paths");
    assert!(report.contains("issue-10"), "{report}");
    assert!(!report.contains("issue-999"), "unnamed snapshot must stay out: {report}");

    let mut store = SqliteStore::open(store_path.to_str().expect("utf-8")).expect("reopen");
    assert!(store.load(&workpiece("issue-10")).expect("load").is_some());
    assert!(store.load(&workpiece("issue-999")).expect("load").is_none());
}

#[test]
fn a_mismatched_sealed_revision_is_refused() {
    // Writing a freshly parsed revision under a surviving sealed pin would
    // make load_revision(pinned) miss, or worse store a different digest.
    let mut store = memory();
    let (draft, _, approval) = sealed_member("issue-12");
    let spec = draft.seal();
    let mut other = revision("issue-12");
    other.problem = "a different revision than the pin".to_owned();
    match import(
        &mut store,
        &ImportRequest { issues: Vec::new(), sealed: vec![SealedWorkpiece { spec, revision: other, approval }] },
    ) {
        Err(ImportError::PinnedDigestMismatch { workpiece }) => assert_eq!(workpiece, "issue-12"),
        other => panic!("expected pinned digest mismatch, got {other:?}"),
    }
}
