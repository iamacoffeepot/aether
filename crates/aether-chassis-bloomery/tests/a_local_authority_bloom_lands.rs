#![cfg(all(unix, feature = "github"))]

//! A bloom runs start to finish against a fleet-local bare authority, with no
//! network: real `SQLite` stores, the local git-data source, the real transform
//! runner pointed at `bloomery-mock-lane`, real capture and publication, a
//! restart between resolve and land, and a mirror double that fails without
//! reversing the land (ADR-0199 slice 1).
//!
//! The proof this file exists to carry: the local-authority shape is a cell of
//! the consolidated scenario harness — backend local, coordinator in-process,
//! lane scripted — not a fourth copy of the boot loop.

mod common;
pub mod harness;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aether_bloomery::testing::digest;
use aether_bloomery::{BloomId, BloomStatus, ProjectedReceipt, ProjectionBackend, Topic, ViewDocument, WorkpieceId};
use aether_bloomery_github::{GithubError, candidate_ref_name};
use aether_chassis_bloomery::bloomery::ProjectionShell;
use aether_chassis_bloomery::bloomery::TopicOutbox;
use aether_chassis_bloomery::bloomery::mock_lane::CANDIDATE_FILE;
use aether_chassis_bloomery::store::SqliteStore;
use aether_data::wire::from_bytes;

use crate::common::repo::Repo;
use crate::harness::{HarnessBuilder, HarnessRoots};

const WORKPIECE: &str = "wp";

#[test]
fn a_local_authority_bloom_lands_after_a_restart_and_a_failing_mirror() {
    let authority = Repo::bare_authority();
    let roots = HarnessRoots::create();

    let (bloom, sealed_on, first_mainline) = {
        // Land is gated off so resolve can be observed. The control core
        // nudges the land reactor the moment review passes; with CAS on, the
        // bloom is Landed before this loop can see Resolved.
        let mut harness =
            HarnessBuilder::local_authority(&authority).roots(&roots).cas_land(false).start("local-authority-1");
        let sealed_on = harness.view().mainline;
        let first_mainline = authority.rev_parse("refs/heads/main");
        let bloom = harness.seal_member(WORKPIECE, digest(0x51));

        harness.pump_until("the bloom resolves against the local authority", |harness| {
            harness.bloom(bloom).status == BloomStatus::Resolved
        });

        assert_eq!(
            authority.rev_parse("refs/heads/main"),
            first_mainline,
            "resolve must not move mainline; land is the compare-and-swap",
        );
        assert_candidate_is_a_commit_wrapping_a_tree(&authority, bloom);
        (bloom, sealed_on, first_mainline)
    };

    let mut harness =
        HarnessBuilder::local_authority(&authority).roots(&roots).cas_land(true).start("local-authority-2");
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Resolved, "the journal replayed the already-produced head");
    harness.land_until(bloom, BloomStatus::Landed);

    assert_ne!(harness.view().mainline, sealed_on, "the receipt advanced coordinator mainline");
    assert_ne!(authority.rev_parse("refs/heads/main"), first_mainline, "the bare authority's mainline moved");
    assert_eq!(authority.git(&["cat-file", "-t", "refs/heads/main"]).trim(), "commit");
    let landed_names = authority.git(&["ls-tree", "-r", "--name-only", "refs/heads/main"]);
    assert!(
        landed_names.lines().any(|name| name == CANDIDATE_FILE),
        "the landed tree carries the mock lane's edit: {landed_names}"
    );

    let receipt = landing_receipt(&roots.store_path());
    assert_eq!(receipt.receipt.bloom, bloom);
    assert_eq!(receipt.receipt.previous_base, sealed_on);
    assert_eq!(receipt.receipt.new_head, harness.view().mainline);
    assert_eq!(receipt.members, [WorkpieceId(WORKPIECE.to_owned())]);

    let landed_head = authority.rev_parse("refs/heads/main");
    let mirror = Arc::new(FailingMirror::new());
    let shell = ProjectionShell::new(Arc::clone(&mirror) as Arc<_>);
    mirror.fail();
    shell.project_receipt(&receipt).expect_err("the first replication is the failure this scenario exists to see");
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "a failed mirror must not un-land the bloom");
    assert_eq!(authority.rev_parse("refs/heads/main"), landed_head, "a failed mirror must not move mainline back");

    mirror.allow();
    shell.project_receipt(&receipt).expect("replication retries after the outage");
    assert_eq!(mirror.receipts(), 1, "the retry delivered the same receipt, once");
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed);
    assert_eq!(authority.rev_parse("refs/heads/main"), landed_head);
}

/// The capture published at the member's candidate ref is a commit whose tree
/// is a tree — the #5025/#5027 pairing. Landing a tree SHA as if it were a
/// commit, or checking one out without the wrapper, is the defect class this
/// hermetic path exists to make CI-visible.
fn assert_candidate_is_a_commit_wrapping_a_tree(authority: &Repo, bloom: BloomId) {
    let candidate = candidate_ref_name(&bloom, WORKPIECE);
    let checkout = authority.rev_parse(&candidate);
    assert_eq!(authority.git(&["cat-file", "-t", &checkout]).trim(), "commit");
    let tree = authority.git(&["rev-parse", &format!("{checkout}^{{tree}}")]);
    assert_eq!(authority.git(&["cat-file", "-t", tree.trim()]).trim(), "tree");
}

fn landing_receipt(store_path: &str) -> ProjectedReceipt {
    let mut store = SqliteStore::open(store_path).expect("the journal reopens");
    let entries = store.drain_topic(Topic::LandingReceipt).expect("the landing-receipt topic drains");
    let entry = entries.first().expect("land emitted a receipt");
    from_bytes(&entry.payload).expect("the receipt decodes")
}

struct FailingMirror {
    fail: AtomicBool,
    receipts: Mutex<usize>,
}

impl FailingMirror {
    fn new() -> Self {
        Self { fail: AtomicBool::new(false), receipts: Mutex::new(0) }
    }

    fn fail(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }

    fn allow(&self) {
        self.fail.store(false, Ordering::SeqCst);
    }

    fn receipts(&self) -> usize {
        *self.receipts.lock().expect("mirror receipt count")
    }
}

impl ProjectionBackend for FailingMirror {
    type Error = GithubError;

    fn reconcile_view(&self, _view: &ViewDocument) -> Result<(), GithubError> {
        Ok(())
    }

    fn project_receipt(&self, _receipt: &ProjectedReceipt) -> Result<(), GithubError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(GithubError::Status { status: 503, body: "mirror unreachable".to_owned() });
        }
        *self.receipts.lock().expect("mirror receipt count") += 1;
        Ok(())
    }

    fn project_commission(&self, _projection: &aether_bloomery::CommissionProjection) -> Result<u64, GithubError> {
        Ok(0)
    }
}
