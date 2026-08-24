//! `LocalGitData` against temporary bare repositories.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::slice::from_ref;
use std::sync::{Arc, Mutex};
use std::thread;

use aether_bloomery::control::{ReconcileOp, reconcile_op};
use aether_bloomery::testing::{digest, membership};
use aether_bloomery::{
    BackendObjectId, BloomDraft, BloomId, BloomRecord, BloomStatus, ClaimOutcome, Correspondence, CorrespondenceError,
    Digest, IntegrateOutcome, LandOutcome, SharedCorrespondence, Snapshot, SourceBackend, WorkpieceId,
};

use super::LocalGitData;
use crate::MainlineRef;
use crate::client::{GitDataApi, GitDataError, MergeResult, RefTxnOp};
use crate::command::run_stdin;
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
    let output = run_stdin(local.repo(), args, stdin).expect("git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A commit minted under an ambient identity and the host clock — a stand-in
/// for the operator commit a bloom's base is, or the lane capture a fold merges
/// in, neither of which the bloomery mints itself.
fn dated_commit(local: &LocalGitData, tree: &str, message: &str) -> String {
    let identity = ["-c", "user.name=fixture", "-c", "user.email=fixture@example.test"];
    git(local, &[&identity[..], &["commit-tree", tree, "-m", message]].concat(), "")
}

/// `sha`'s committer timestamp in whole seconds since the epoch.
fn commit_stamp(local: &LocalGitData, sha: &str) -> String {
    git(local, &["show", "--no-patch", "--format=%ct", sha, "--"], "")
}

/// `sha`'s raw `author` header line.
fn author_line(local: &LocalGitData, sha: &str) -> String {
    let body = git(local, &["cat-file", "commit", sha], "");
    body.lines().find(|line| line.starts_with("author ")).expect("a commit carries an author line").to_owned()
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
    thread::scope(|scope| {
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
fn a_minted_commit_is_authored_by_the_bloomery_at_the_moment_it_inherits() {
    // Tripwire: the author line is what a reader sees on the landed day — the
    // roll rewrites the committer side when it linearizes the day onto main and
    // keeps the author verbatim. An epoch-zero date under an `.invalid` domain
    // renders there as an unattributed 1970 commit, so both the address and the
    // moment are pinned. The moment is the parent's, not the clock's, because
    // the sha has to stay a pure function of the inputs.
    let (_root, local) = open_temp();
    let tree = git(&local, &["mktree"], "");
    let parent = dated_commit(&local, &tree, "base");
    let stamp = commit_stamp(&local, &parent);

    let commit = local.create_commit("bloomery integrate", &tree, from_ref(&parent)).expect("integrate");

    assert_eq!(
        author_line(&local, &commit.sha),
        format!("author bloomery <bloomery@iamateapot.dev> {stamp} +0000"),
        "the bloomery authors its own commits, at the moment it inherits"
    );
    assert_ne!(stamp, "0", "the fixture parent carries a real moment to inherit");
}

#[test]
fn re_minting_over_the_same_parent_returns_the_same_sha_and_the_same_date() {
    // Tripwire: `GitSource::integrate` recovers from a fault between its commit
    // and its ref update only because the retry re-creates a byte-identical
    // commit and git hands back the same sha. Two mints a moment apart tie
    // under a wall-clock date too, so the load-bearing half of this is the
    // date itself: an inherited one is in the past, a clock-read one is now.
    let (_root, local) = open_temp();
    let tree = git(&local, &["mktree"], "");
    let parent = dated_commit(&local, &tree, "base");
    let stamp = commit_stamp(&local, &parent);

    let first = local.create_commit("bloomery integrate", &tree, from_ref(&parent)).expect("first");
    let second = local.create_commit("bloomery integrate", &tree, from_ref(&parent)).expect("second");

    assert_eq!(first.sha, second.sha, "one input mints one commit");
    assert!(author_line(&local, &first.sha).ends_with(&format!("{stamp} +0000")), "the date is the parent's");
}

#[test]
fn a_two_parent_fold_inherits_the_newest_parent_whichever_side_it_is_on() {
    // Tripwire: a fold's merge carries the branch it advances and the candidate
    // capture it merges in, and the capture is when the work was actually
    // produced. Taking the newest is what keeps the dates along a branch from
    // going backwards; taking a fixed side would stamp the merge with whichever
    // parent happened to be listed first.
    let (_root, local) = open_temp();
    let tree = git(&local, &["mktree"], "");
    let dated = dated_commit(&local, &tree, "candidate");
    let stamp = commit_stamp(&local, &dated);
    let epoch = local.create_commit("integration", EMPTY_TREE, &[]).expect("epoch parent").sha;

    for parents in [[epoch.clone(), dated.clone()], [dated, epoch]] {
        let commit = local.create_commit("bloomery fold heads/candidate", &tree, &parents).expect("fold");
        assert!(
            author_line(&local, &commit.sha).ends_with(&format!("{stamp} +0000")),
            "the newest parent supplies the moment, listed {parents:?}"
        );
    }
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
        other @ LandOutcome::BaseMoved { .. } => panic!("expected Landed, got {other:?}"),
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
        other @ ClaimOutcome::Acquired => panic!("expected Held, got {other:?}"),
    }
}

const ADMISSION_REF: &str = "bloomery/admission/mainline";

fn claim_ref(workpiece: &str) -> String {
    format!("bloomery/claims/{workpiece}")
}

fn git_source(local: LocalGitData) -> GitSource<LocalGitData> {
    GitSource::new(local, MapCorrespondence::new(), true, MainlineRef::default())
}

#[test]
fn transact_refs_refuses_two_ops_on_the_same_ref() {
    // Tripwire: git's `update-ref --stdin` refuses two ops on one name, which
    // is the pre-fix `release_targets` batch (Update-to-tombstone then Delete).
    let (_root, local) = open_temp();
    let live = local.create_commit("claim", EMPTY_TREE, &[]).expect("live");
    local.create_ref(ADMISSION_REF, &live.sha).expect("point admission");
    let tombstone = local.create_commit("tombstone", EMPTY_TREE, from_ref(&live.sha)).expect("tombstone");

    let error = local
        .transact_refs(&[
            RefTxnOp::Update { name: ADMISSION_REF.into(), sha: tombstone.sha.clone(), expected: live.sha.clone() },
            RefTxnOp::Delete { name: ADMISSION_REF.into(), expected: tombstone.sha },
        ])
        .expect_err("git refuses two ops on one ref in one transaction");
    assert!(matches!(error, GitDataError::Command(_)), "same-ref batches are Command, not a lost CAS; got {error:?}");
    let text = error.to_string();
    assert!(text.contains("multiple updates"), "{text}");
    assert_eq!(ref_sha(&local, ADMISSION_REF), live.sha, "the refused transaction left the ref in place");
}

#[test]
fn create_ref_with_a_nonexistent_object_is_missing_object() {
    let (_root, local) = open_temp();
    let missing = "a".repeat(40);
    match local.create_ref("heads/ghost", &missing) {
        Err(GitDataError::MissingObject(detail)) => {
            assert!(detail.to_ascii_lowercase().contains("nonexistent object"), "{detail}");
        }
        other => panic!("a missing sha is MissingObject, got {other:?}"),
    }
}

#[test]
fn create_ref_with_a_bad_name_is_a_command_fault() {
    let (_root, local) = open_temp();
    let commit = local.create_commit("seed", EMPTY_TREE, &[]).expect("commit");
    match local.create_ref("heads/bad name", &commit.sha) {
        Err(GitDataError::Command(detail)) => {
            assert!(detail.to_ascii_lowercase().contains("bad name"), "{detail}");
        }
        other => panic!("a bad ref name is Command, got {other:?}"),
    }
}

#[test]
fn is_ancestor_reports_missing_object_for_an_unknown_sha() {
    let (_root, local) = open_temp();
    let commit = local.create_commit("seed", EMPTY_TREE, &[]).expect("commit");
    let missing = "a".repeat(40);
    match local.is_ancestor(&missing, &commit.sha) {
        Err(GitDataError::MissingObject(_)) => {}
        other => panic!("an unknown sha is MissingObject, got {other:?}"),
    }
}

#[test]
fn is_ancestor_is_false_for_unrelated_commits() {
    let (_root, local) = open_temp();
    let a = local.create_commit("a", EMPTY_TREE, &[]).expect("a");
    let b = local.create_commit("b", EMPTY_TREE, &[]).expect("b");
    assert!(!local.is_ancestor(&a.sha, &b.sha).expect("both objects exist"));
    assert!(local.is_ancestor(&a.sha, &a.sha).expect("equal shas are ancestors of themselves"));
}

#[test]
fn release_seal_tombstones_then_deletes_the_owned_refs() {
    let (_root, local) = open_temp();
    let source = git_source(local.clone());
    let owner = BloomId(digest(1));
    let workpiece = WorkpieceId("wp-1".into());
    assert_eq!(source.claim_seal(&owner, from_ref(&workpiece)).expect("acquire"), ClaimOutcome::Acquired);

    assert_eq!(source.release_seal(&owner, from_ref(&workpiece)).expect("release"), ClaimOutcome::Acquired);
    assert!(ref_absent(&local, &claim_ref("wp-1")), "owned member released");
    assert!(ref_absent(&local, ADMISSION_REF), "owned admission released");

    let next = BloomId(digest(2));
    assert_eq!(source.claim_seal(&next, from_ref(&workpiece)).expect("next seal"), ClaimOutcome::Acquired);
}

#[test]
fn boot_reconcile_re_releases_a_landed_blooms_stranded_refs() {
    let (_root, local) = open_temp();
    let source = git_source(local.clone());
    let spec = BloomDraft { proposals: vec![membership("wp-1", 11)], ..Default::default() }.seal();
    let bloom = spec.id();
    let workpiece = WorkpieceId("wp-1".into());
    assert_eq!(source.claim_seal(&bloom, from_ref(&workpiece)).expect("acquire"), ClaimOutcome::Acquired);
    assert!(
        !ref_absent(&local, &claim_ref("wp-1")) && !ref_absent(&local, ADMISSION_REF),
        "the land stranded the refs"
    );

    let mut snapshot = Snapshot::default();
    snapshot.blooms.insert(bloom, BloomRecord { status: BloomStatus::Landed, ..BloomRecord::empty(spec) });
    let ReconcileOp::Release(_) =
        reconcile_op(snapshot.blooms.get(&bloom).expect("landed")).expect("plans").expect("encodes")
    else {
        panic!("a Landed journal record must re-release");
    };

    assert_eq!(source.release_seal(&bloom, from_ref(&workpiece)).expect("boot release"), ClaimOutcome::Acquired);
    assert!(ref_absent(&local, &claim_ref("wp-1")), "member was re-released at boot");
    assert!(ref_absent(&local, ADMISSION_REF), "admission was re-released at boot");

    assert_eq!(
        source.release_seal(&bloom, from_ref(&workpiece)).expect("repeat"),
        ClaimOutcome::Acquired,
        "a repeated reconcile over now-absent refs is the idempotent no-op"
    );
    assert!(ref_absent(&local, &claim_ref("wp-1")), "a repeated reconcile re-creates nothing");
}

/// The checked-in attribute file, so these tests exercise the pattern that is
/// actually shipped rather than a restatement of it.
const GITATTRIBUTES: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.gitattributes"));

/// Base, ours, and theirs for one crate root, each a full tree carrying the
/// checked-in `.gitattributes` beside `crates/example/src/lib.rs`.
fn reexport_tree(local: &LocalGitData, lib: &str) -> String {
    let attributes = git(local, &["hash-object", "-w", "--stdin"], GITATTRIBUTES);
    let blob = git(local, &["hash-object", "-w", "--stdin"], lib);
    let src = git(local, &["mktree"], &format!("100644 blob {blob}\tlib.rs\n"));
    let example = git(local, &["mktree"], &format!("040000 tree {src}\tsrc\n"));
    let crates = git(local, &["mktree"], &format!("040000 tree {example}\texample\n"));
    git(local, &["mktree"], &format!("100644 blob {attributes}\t.gitattributes\n040000 tree {crates}\tcrates\n"))
}

fn fold(local: &LocalGitData, base: &str, ours: &str, theirs: &str) -> MergeResult {
    let base_sha = local.create_commit("base", &reexport_tree(local, base), &[]).expect("base").sha;
    let ours_sha = local.create_commit("ours", &reexport_tree(local, ours), from_ref(&base_sha)).expect("ours").sha;
    let theirs_sha =
        local.create_commit("theirs", &reexport_tree(local, theirs), from_ref(&base_sha)).expect("theirs").sha;
    local.create_ref("heads/fold-base", &ours_sha).expect("fold-base");
    local.create_ref("heads/fold-side", &theirs_sha).expect("fold-side");
    local.merge("heads/fold-base", "heads/fold-side", "fold").expect("merge runs")
}

fn lib_at(local: &LocalGitData, commit: &str) -> String {
    git(local, &["show", &format!("{commit}:crates/example/src/lib.rs")], "")
}

#[test]
fn two_members_each_appending_a_reexport_fold_without_a_reconcile_lane() {
    // Pre-fix: the same two commits conflict, and the reconcile lane that
    // repairs them costs a model dispatch on a merge whose answer is
    // determined. `--write-tree` reads the driver out of the repository config
    // and `.gitattributes` out of the merged trees.
    let (_root, local) = open_temp();
    let base = "pub use values::Alpha;\npub use values::Gamma;\n";
    let ours = "pub use values::Alpha;\npub use values::Beta;\npub use values::Gamma;\n";
    let theirs = "pub use values::Alpha;\npub use values::Delta;\npub use values::Gamma;\n";

    let merged = match fold(&local, base, ours, theirs) {
        MergeResult::Merged(commit) => commit,
        other => panic!("the driver must resolve two appended re-exports, got {other:?}"),
    };

    // Byte-identical to what `cargo fmt` would leave: union, sorted, no
    // conflict markers. An unsorted union is a `reorder_imports` finding, which
    // would turn a resolved conflict into a failed verify.
    assert_eq!(
        lib_at(&local, &merged.sha),
        "pub use values::Alpha;\npub use values::Beta;\npub use values::Delta;\npub use values::Gamma;",
    );
}

#[test]
fn a_change_that_is_not_an_insertion_still_conflicts() {
    // Tripwire: the driver's whole safety argument is that it declines
    // everything it does not recognize. One side rewriting a base line is a
    // real edit needing judgement, and it must still reach reconcile.
    let (_root, local) = open_temp();
    let base = "pub use values::Alpha;\npub use values::Gamma;\n";
    let ours = "pub use values::Alpha;\npub use values::Beta;\npub use values::Gamma;\n";
    let theirs = "pub use values::Alpha;\npub use values::Omega;\n";

    match fold(&local, base, ours, theirs) {
        MergeResult::Conflict { paths, .. } => {
            assert_eq!(paths, ["crates/example/src/lib.rs"]);
        }
        other => panic!("a rewritten line must not be resolved, got {other:?}"),
    }
}

#[test]
fn a_conflicting_block_that_is_not_a_reexport_list_still_conflicts() {
    // The trust boundary: `.gitattributes` lives in the tree, so a candidate
    // can point this driver at a path it was never meant for. Pointed at one,
    // it must be harmless — anything but a `pub use <path>;` line refuses.
    let (_root, local) = open_temp();
    let base = "pub mod values;\n";
    let ours = "pub mod values;\npub mod alpha;\n";
    let theirs = "pub mod values;\npub mod beta;\n";

    match fold(&local, base, ours, theirs) {
        MergeResult::Conflict { paths, .. } => {
            assert_eq!(paths, ["crates/example/src/lib.rs"]);
        }
        other => panic!("a non-re-export block must not be resolved, got {other:?}"),
    }
}

#[test]
fn a_conflicted_merge_names_only_the_files_that_conflicted() {
    // Pre-fix: merge-tree's informational section (`Auto-merging`,
    // `CONFLICT (content)`) and the cleanly merged file survived into
    // `Conflict.paths`. The repair work order then serializes siblings that
    // share only a clean auto-merge.
    let (_root, local) = open_temp();
    let tree = |a: &str, common: &str| {
        let a_blob = git(&local, &["hash-object", "-w", "--stdin"], a);
        let common_blob = git(&local, &["hash-object", "-w", "--stdin"], common);
        git(&local, &["mktree"], &format!("100644 blob {a_blob}\ta.txt\n100644 blob {common_blob}\tcommon.txt\n"))
    };

    let base = local.create_commit("base", &tree("base-a", "base-common"), &[]).expect("base").sha;
    let ours = local.create_commit("ours", &tree("ours-a", "new-common"), from_ref(&base)).expect("ours").sha;
    let theirs = local.create_commit("theirs", &tree("theirs-a", "new-common"), from_ref(&base)).expect("theirs").sha;
    local.create_ref("heads/base", &ours).expect("base");
    local.create_ref("heads/side", &theirs).expect("side");

    match local.merge("heads/base", "heads/side", "fold").expect("conflict is Ok") {
        MergeResult::Conflict { paths, .. } => assert_eq!(paths, ["a.txt"]),
        other => panic!("expected Conflict, got {other:?}"),
    }
}
