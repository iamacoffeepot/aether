//! `LocalGitData` against temporary bare repositories.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::slice::from_ref;
use std::sync::{Arc, Mutex};

use aether_bloomery::{
    BackendObjectId, BloomId, ClaimOutcome, Correspondence, CorrespondenceError, Digest, IntegrateOutcome, LandOutcome,
    SharedCorrespondence, SourceBackend, WorkpieceId,
};

use super::LocalGitData;
use crate::MainlineRef;
use crate::client::{GitDataApi, GitDataError, MergeResult, RefTxnOp};
use crate::correspondence::GitObjectId;
use crate::source::{EMPTY_TREE, GitSource};

struct MapCorrespondence {
    pairs: Mutex<HashMap<Digest, BackendObjectId>>,
}

impl MapCorrespondence {
    fn new() -> Arc<Self> {
        Arc::new(Self { pairs: Mutex::new(HashMap::new()) })
    }
}

impl Correspondence for MapCorrespondence {
    fn record(&self, digest: &Digest, object: &BackendObjectId) -> Result<(), CorrespondenceError> {
        self.pairs.lock().expect("correspondence mutex").insert(*digest, object.clone());
        Ok(())
    }

    fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError> {
        Ok(self.pairs.lock().expect("correspondence mutex").get(digest).cloned())
    }

    fn resolve_digest(&self, object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError> {
        Ok(self
            .pairs
            .lock()
            .expect("correspondence mutex")
            .iter()
            .find_map(|(digest, stored)| (stored == object).then_some(*digest)))
    }
}

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn init_bare(path: &Path) {
    let status = Command::new("git").args(["init", "--bare", "-b", "main"]).arg(path).status().expect("git init");
    assert!(status.success(), "git init --bare {path:?}");
}

fn open_temp() -> (tempfile::TempDir, LocalGitData) {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = root.path().join("repo.git");
    init_bare(&repo);
    let local = LocalGitData::open(repo.canonicalize().expect("absolute repo")).expect("open local git-data");
    (root, local)
}

fn commit_tree(local: &LocalGitData, message: &str, payload: &str) -> (String, String) {
    let blob = git(local, &["hash-object", "-w", "--stdin"], payload);
    let tree = git(local, &["mktree"], &format!("100644 blob {blob}\t{payload}.txt\n"));
    let commit = local.create_commit(message, &tree, &[]).expect("commit-tree");
    (commit.sha, tree)
}

fn git(local: &LocalGitData, args: &[&str], stdin: &str) -> String {
    let output = super::command::run_stdin(local.repo(), args, stdin).expect("git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn ref_sha(local: &LocalGitData, name: &str) -> String {
    local.get_ref(name).expect("get_ref").expect("ref is present").sha
}

fn ref_absent(local: &LocalGitData, name: &str) -> bool {
    local.get_ref(name).expect("get_ref").is_none()
}

#[test]
fn open_refuses_a_relative_path() {
    match LocalGitData::open("relative/repo.git") {
        Err(GitDataError::Command(detail)) => {
            assert!(detail.contains("absolute"), "{detail}");
            assert!(detail.contains("file://"), "{detail}");
        }
        other => panic!("expected Command, got {other:?}"),
    }
}

#[test]
fn open_accepts_an_absolute_path_containing_spaces() {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = root.path().join("repo with spaces.git");
    init_bare(&repo);
    let local = LocalGitData::open(repo.canonicalize().expect("absolute")).expect("open spaced path");
    let commit = local.create_commit("seed", EMPTY_TREE, &[]).expect("commit");
    local.create_ref("heads/main", &commit.sha).expect("create main");
    assert_eq!(ref_sha(&local, "heads/main"), commit.sha);
}

#[test]
fn create_get_list_and_delete_refs() {
    let (_root, local) = open_temp();
    let commit = local.create_commit("seed", EMPTY_TREE, &[]).expect("commit");

    let created = local.create_ref("heads/topic", &commit.sha).expect("create");
    assert_eq!(created.sha, commit.sha);
    assert_eq!(ref_sha(&local, "heads/topic"), commit.sha);
    assert!(ref_absent(&local, "heads/missing"));

    match local.create_ref("heads/topic", &commit.sha) {
        Err(GitDataError::RefConflict(_)) => {}
        other => panic!("existing create is RefConflict, got {other:?}"),
    }

    let listed = local.list_matching_refs("heads/").expect("list");
    assert!(listed.iter().any(|git_ref| git_ref.name == "heads/topic"));

    local.delete_ref("heads/topic").expect("delete");
    assert!(ref_absent(&local, "heads/topic"));
    local.delete_ref("heads/topic").expect("idempotent delete");
}

#[test]
fn compare_and_swap_is_expected_value_not_fast_forward() {
    let (_root, local) = open_temp();
    let a = local.create_commit("a", EMPTY_TREE, &[]).expect("a");
    let b = local.create_commit("b", EMPTY_TREE, from_ref(&a.sha)).expect("b");
    local.create_ref("heads/main", &a.sha).expect("point at a");

    local.compare_and_swap_ref("heads/main", &b.sha, &a.sha).expect("A -> B");
    match local.compare_and_swap_ref("heads/main", &a.sha, &a.sha) {
        Err(GitDataError::RefConflict(_)) => {}
        other => panic!("stale expected-old must lose, got {other:?}"),
    }
    assert_eq!(ref_sha(&local, "heads/main"), b.sha);
}

#[test]
fn compare_and_swap_race_lets_exactly_one_writer_win() {
    // Tripwire: expected-value CAS, not fast-forward-only. B and C are a
    // parent/child pair, so after A->B a fast-forward update to C would
    // succeed. The loser still names expected=A, so only one write may land.
    let (_root, local) = open_temp();
    let a = local.create_commit("a", EMPTY_TREE, &[]).expect("a");
    let b = local.create_commit("b", EMPTY_TREE, from_ref(&a.sha)).expect("b");
    let c = local.create_commit("c", EMPTY_TREE, from_ref(&b.sha)).expect("c");
    local.create_ref("heads/main", &a.sha).expect("point at a");

    let first = local.clone();
    let second = local.clone();
    let expected = a.sha;
    let to_b = b.sha.clone();
    let to_c = c.sha.clone();
    std::thread::scope(|scope| {
        let left = scope.spawn(|| first.compare_and_swap_ref("heads/main", &to_b, &expected));
        let right = scope.spawn(|| second.compare_and_swap_ref("heads/main", &to_c, &expected));
        let wins =
            [&left.join().expect("left"), &right.join().expect("right")].iter().filter(|result| result.is_ok()).count();
        assert_eq!(wins, 1, "exactly one of two CAS writes from the same expected-old may succeed");
    });
    let head = ref_sha(&local, "heads/main");
    assert!(head == b.sha || head == c.sha, "main landed on one of the two candidates");
}

#[test]
fn transact_refs_is_all_or_nothing_under_a_mid_batch_conflict() {
    let (_root, local) = open_temp();
    let first = local.create_commit("one", EMPTY_TREE, &[]).expect("one");
    let second = local.create_commit("two", EMPTY_TREE, &[]).expect("two");
    let third = local.create_commit("three", EMPTY_TREE, &[]).expect("three");
    local.create_ref("heads/held", &second.sha).expect("pre-existing hold");

    let error = local
        .transact_refs(&[
            RefTxnOp::Create { name: "heads/fresh-a".into(), sha: first.sha },
            RefTxnOp::Create { name: "heads/held".into(), sha: second.sha.clone() },
            RefTxnOp::Create { name: "heads/fresh-b".into(), sha: third.sha },
        ])
        .expect_err("the existing name must abort the transaction");
    assert!(matches!(error, GitDataError::RefConflict(_)), "{error:?}");
    assert!(ref_absent(&local, "heads/fresh-a"), "the first create rolled back");
    assert!(ref_absent(&local, "heads/fresh-b"), "the trailing create never landed");
    assert_eq!(ref_sha(&local, "heads/held"), second.sha);
}

#[test]
fn create_commit_is_deterministic_and_get_commit_reads_it_back() {
    let (_root, local) = open_temp();
    let first = local.create_commit("same", EMPTY_TREE, &[]).expect("first");
    let second = local.create_commit("same", EMPTY_TREE, &[]).expect("second");
    assert_eq!(first.sha, second.sha, "pinned identity makes a retry byte-identical");
    let read = local.get_commit(&first.sha).expect("cat-file");
    assert_eq!(read.tree, EMPTY_TREE);
    assert_eq!(read.message, "same");
}

#[test]
fn merge_writes_a_tree_and_reports_conflict_from_exit_status() {
    let (_root, local) = open_temp();
    let (base_sha, _) = commit_tree(&local, "base", "shared");
    local.create_ref("heads/base", &base_sha).expect("base");
    local.create_ref("heads/side", &base_sha).expect("side");

    let left_blob = git(&local, &["hash-object", "-w", "--stdin"], "left");
    let left_tree = git(&local, &["mktree"], &format!("100644 blob {left_blob}\tshared.txt\n"));
    let left = local.create_commit("left", &left_tree, from_ref(&base_sha)).expect("left");
    local.compare_and_swap_ref("heads/base", &left.sha, &base_sha).expect("advance base");

    let right_blob = git(&local, &["hash-object", "-w", "--stdin"], "right");
    let right_tree = git(&local, &["mktree"], &format!("100644 blob {right_blob}\tshared.txt\n"));
    let right = local.create_commit("right", &right_tree, &[base_sha]).expect("right");
    local.compare_and_swap_ref("heads/side", &right.sha, &ref_sha(&local, "heads/side")).ok();
    local.update_ref("heads/side", &right.sha, true).expect("point side");

    match local.merge("heads/base", "heads/side", "fold").expect("conflict is Ok") {
        MergeResult::Conflict { paths, .. } => {
            assert!(paths.iter().any(|path| path.contains("shared")), "{paths:?}");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    let (clean_base, _) = commit_tree(&local, "clean-base", "keep");
    local.create_ref("heads/clean-base", &clean_base).expect("clean-base");
    let extra = git(&local, &["hash-object", "-w", "--stdin"], "added");
    let extra_tree = git(
        &local,
        &["mktree"],
        &format!(
            "100644 blob {}\tkeep.txt\n100644 blob {extra}\textra.txt\n",
            git(&local, &["hash-object", "-w", "--stdin"], "keep")
        ),
    );
    let extra_commit = local.create_commit("extra", &extra_tree, from_ref(&clean_base)).expect("extra");
    local.create_ref("heads/clean-head", &extra_commit.sha).expect("clean-head");
    match local.merge("heads/clean-base", "heads/clean-head", "merge clean").expect("clean merge") {
        MergeResult::Merged(commit) => {
            assert_eq!(ref_sha(&local, "heads/clean-base"), commit.sha);
        }
        other => panic!("expected Merged, got {other:?}"),
    }
}

#[test]
fn git_source_snapshot_land_and_claim_run_against_the_local_backend() {
    let (_root, local) = open_temp();
    let correspondence: SharedCorrespondence = MapCorrespondence::new();
    let (base_sha, tree_sha) = commit_tree(&local, "base", "root");
    let base = digest(10);
    let tree = digest(11);
    correspondence
        .record(&base, &BackendObjectId::from(GitObjectId::from_hex(&base_sha).expect("base sha")))
        .expect("record base");
    correspondence
        .record(&tree, &BackendObjectId::from(GitObjectId::from_hex(&tree_sha).expect("tree sha")))
        .expect("record tree");
    local.create_ref("heads/main", &base_sha).expect("main");

    let source = GitSource::new(local.clone(), Arc::clone(&correspondence), true, MainlineRef::default());
    let snapshot = source.snapshot(&base).expect("snapshot");
    assert_eq!(snapshot.head, base);
    assert_eq!(snapshot.tree, tree);

    let bloom = BloomId(digest(1));
    let position = source.integration_checkpoint(&bloom, &base).expect("checkpoint");
    assert_eq!(position.checkpoint.tree, tree);
    assert!(position.head.is_none(), "a freshly bootstrapped branch has no landable head");

    let candidate = digest(50);
    let candidate_blob = git(&local, &["hash-object", "-w", "--stdin"], "candidate");
    let candidate_tree = git(&local, &["mktree"], &format!("100644 blob {candidate_blob}\tcandidate.txt\n"));
    correspondence
        .record(&candidate, &BackendObjectId::from(GitObjectId::from_hex(&candidate_tree).expect("candidate sha")))
        .expect("record candidate");
    let integrated = source.integrate(&bloom, &candidate, &position.checkpoint).expect("integrate");
    let IntegrateOutcome::Integrated { head, .. } = integrated else {
        panic!("expected Integrated, got {integrated:?}");
    };

    match source.land(&bloom, &base, &head).expect("land") {
        LandOutcome::Landed { new_head } => assert_eq!(new_head, head),
        other => panic!("expected Landed, got {other:?}"),
    }

    let recovered = GitSource::new(local, Arc::clone(&correspondence), true, MainlineRef::default())
        .integration_checkpoint(&bloom, &base)
        .expect("recover");
    assert_eq!(recovered.head, Some(head), "ADR-0152 recovery returns the already-produced head");

    let claimant = BloomId(digest(2));
    let workpiece = WorkpieceId("wp-1".into());
    assert_eq!(source.claim_seal(&claimant, from_ref(&workpiece)).expect("acquire"), ClaimOutcome::Acquired);
    let other = BloomId(digest(3));
    match source.claim_seal(&other, &[workpiece]).expect("held") {
        ClaimOutcome::Held { held_by, .. } => assert_eq!(held_by, claimant),
        other => panic!("expected Held, got {other:?}"),
    }
}
