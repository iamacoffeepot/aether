//! Trait-contract conformance for [`LocalGitData`] and [`FakeGithub`].
//!
//! Every row is a divergence the #5179 audit found: the two backends must
//! classify the same repository state onto the same [`GitDataError`] variant.
//! The GitHub REST adapter is out of this suite (network); its remaining
//! divergences are documented on [`GitDataApi`].

use std::path::Path;
use std::process::Command;
use std::slice::from_ref;

use aether_bloomery_git::client::{GitDataApi, GitDataError, RefTxnOp};
use aether_bloomery_git::local::LocalGitData;
use aether_bloomery_git::source::EMPTY_TREE;
use aether_bloomery_git::testing::FakeGithub;

fn init_bare(path: &Path) {
    let status = Command::new("git").args(["init", "--bare", "-b", "main"]).arg(path).status().expect("git init");
    assert!(status.success(), "git init --bare {}", path.display());
}

fn with_each_backend(test: impl Fn(&str, &dyn GitDataApi)) {
    {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo.git");
        init_bare(&repo);
        let local = LocalGitData::open(repo.canonicalize().expect("absolute")).expect("open local git-data");
        test("LocalGitData", &local);
    }
    {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo.git");
        init_bare(&repo);
        let abs = repo.canonicalize().expect("absolute");
        // Materialize the empty tree `create_commit` names, the same way
        // `LocalGitData::open` does, so both backends share one object floor.
        LocalGitData::open(&abs).expect("materialize empty tree");
        test("FakeGithub", &FakeGithub::new().with_object_repo(abs));
    }
}

fn seed(git: &dyn GitDataApi) -> String {
    git.create_commit("seed", EMPTY_TREE, &[]).expect("seed commit").sha
}

#[test]
fn sha_naming_no_object_is_missing_object() {
    with_each_backend(|label, git| {
        let missing = "a".repeat(40);
        match git.create_ref("heads/ghost", &missing) {
            Err(GitDataError::MissingObject(_)) => {}
            other => panic!("{label}: a missing sha is MissingObject, got {other:?}"),
        }
    });
}

#[test]
fn invalid_ref_name_is_a_command_fault() {
    with_each_backend(|label, git| {
        let sha = seed(git);
        match git.create_ref("heads/bad name", &sha) {
            Err(GitDataError::Command(_)) => {}
            other => panic!("{label}: a bad ref name is Command, got {other:?}"),
        }
    });
}

#[test]
fn force_update_of_an_absent_ref_creates_it() {
    with_each_backend(|label, git| {
        let sha = seed(git);
        let created = git
            .update_ref("heads/brand-new", &sha, true)
            .unwrap_or_else(|error| panic!("{label}: force on an absent ref creates it, got {error:?}"));
        assert_eq!(created.sha, sha, "{label}");
        assert_eq!(
            git.get_ref("heads/brand-new").expect(label).expect("created").sha,
            sha,
            "{label}: the force-created ref is readable"
        );
        match git.update_ref("heads/still-missing", &sha, false) {
            Err(GitDataError::MissingObject(_)) => {}
            other => panic!("{label}: non-force on an absent ref is MissingObject, got {other:?}"),
        }
    });
}

#[test]
fn same_ref_batch_is_a_command_fault_and_leaves_the_ref() {
    // Tripwire: git's `update-ref --stdin` refuses two ops on one name, which
    // is the pre-fix `release_targets` batch (Update-to-tombstone then Delete).
    // The in-memory fake used to apply the pair; this row fails against that
    // fake and passes once it refuses the same way git does.
    with_each_backend(|label, git| {
        let live = seed(git);
        git.create_ref("heads/held", &live).unwrap_or_else(|error| panic!("{label}: create held: {error}"));
        let tombstone = git.create_commit("tombstone", EMPTY_TREE, from_ref(&live)).expect(label).sha;
        let error = git
            .transact_refs(&[
                RefTxnOp::Update { name: "heads/held".into(), sha: tombstone.clone(), expected: live.clone() },
                RefTxnOp::Delete { name: "heads/held".into(), expected: tombstone },
            ])
            .expect_err(label);
        assert!(
            matches!(error, GitDataError::Command(_)),
            "{label}: same-ref batches are Command, not a lost CAS; got {error:?}"
        );
        assert_eq!(
            git.get_ref("heads/held").expect(label).expect("still there").sha,
            live,
            "{label}: the refused transaction left the ref in place"
        );
    });
}

#[test]
fn batch_delete_of_an_absent_ref_is_missing_object() {
    with_each_backend(|label, git| {
        let sha = seed(git);
        match git.transact_refs(&[RefTxnOp::Delete { name: "heads/never-existed".into(), expected: sha }]) {
            Err(GitDataError::MissingObject(_)) => {}
            other => panic!("{label}: batch delete of an absent ref is MissingObject, got {other:?}"),
        }
    });
}

#[test]
fn is_ancestor_with_a_missing_object_is_an_error() {
    with_each_backend(|label, git| {
        let sha = seed(git);
        let missing = "a".repeat(40);
        match git.is_ancestor(&missing, &sha) {
            Err(GitDataError::MissingObject(_)) => {}
            other => panic!("{label}: an unknown sha is MissingObject, not Ok(false); got {other:?}"),
        }
    });
}

#[test]
fn merge_onto_a_nonexistent_base_is_missing_object() {
    with_each_backend(|label, git| {
        let sha = seed(git);
        git.create_ref("heads/main", &sha).unwrap_or_else(|error| panic!("{label}: create main: {error}"));
        match git.merge("heads/ghost", "heads/main", "fold") {
            Err(GitDataError::MissingObject(_)) => {}
            other => panic!("{label}: merge onto a missing base is MissingObject, got {other:?}"),
        }
    });
}
