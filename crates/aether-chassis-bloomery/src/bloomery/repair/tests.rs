//! Coverage the repair surface owns: digest mint-and-match against the canned
//! capture pair, and the from-commit refusals that name their precondition.

use aether_bloomery::{BackendObjectId, Correspondence};

use super::{candidate_tree_digest, capture_commit_digest};
use crate::store::SqliteCorrespondence;

// The canned capture pair the executor tests mint — twenty `0xcc` commit
// bytes and twenty `0xdd` tree bytes, SHA-1 shaped. The mint-and-match the
// operator used to reconstruct by hand is exactly: derive both digests, record
// them, resolve them back. If the domain tags or the hash recipe move, this
// pair stops resolving and the next hand repair would compute the wrong hex.
fn canned_tree() -> BackendObjectId {
    BackendObjectId::new(vec![0xdd; 20])
}

fn canned_commit() -> BackendObjectId {
    BackendObjectId::new(vec![0xcc; 20])
}

#[test]
fn a_canned_capture_pair_mints_and_matches_its_correspondence() {
    // Tripwire (#5032): the digest recipe the executor stamps on a capture
    // must be the one a repair-from-commit reuses. A tag or hash change that
    // left the executor compiling and this function drifting would make the
    // operator's derived hex miss the row the lane already wrote.
    let store = SqliteCorrespondence::open(":memory:").unwrap();
    let tree = canned_tree();
    let commit = canned_commit();
    let tree_digest = candidate_tree_digest(&tree);
    let checkout_digest = capture_commit_digest(&commit);

    assert_ne!(tree_digest, checkout_digest, "the two axes are domain-separated even over distinct bytes");
    store.record(&tree_digest, &tree).unwrap();
    store.record(&checkout_digest, &commit).unwrap();

    assert_eq!(
        store.resolve_backend_object(&tree_digest).unwrap().as_ref(),
        Some(&tree),
        "the minted tree digest must resolve to the tree bytes it was derived from",
    );
    assert_eq!(
        store.resolve_backend_object(&checkout_digest).unwrap().as_ref(),
        Some(&commit),
        "the minted checkout digest must resolve to the commit bytes it was derived from",
    );
    assert_eq!(store.resolve_digest(&tree).unwrap(), Some(tree_digest));
    assert_eq!(store.resolve_digest(&commit).unwrap(), Some(checkout_digest));
}

#[cfg(feature = "github")]
mod prepare {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;

    use aether_bloomery::{BackendObjectId, BloomId, Correspondence, Digest};
    use aether_bloomery_github::candidate_ref_name;
    use tempfile::TempDir;

    use super::super::{CandidateSource, PrepareError, prepare_candidate};
    use super::candidate_tree_digest;
    use crate::bloomery::CandidatePush;
    use crate::store::SqliteCorrespondence;

    struct RecordingPush {
        pushed: Mutex<Vec<(String, String)>>,
    }

    impl CandidatePush for RecordingPush {
        fn push(&self, commit_hex: &str, target_ref: &str) -> Result<(), String> {
            self.pushed.lock().unwrap().push((commit_hex.to_owned(), target_ref.to_owned()));
            Ok(())
        }
    }

    struct FailingPush;

    impl CandidatePush for FailingPush {
        fn push(&self, _commit_hex: &str, target_ref: &str) -> Result<(), String> {
            Err(format!("origin refused {target_ref}"))
        }
    }

    fn bloom() -> BloomId {
        BloomId(Digest::from_bytes([0xab; 32]))
    }

    fn repo_with_commit() -> (TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "--quiet"]);
        run_git(dir.path(), &["config", "--local", "user.name", "repair"]);
        run_git(dir.path(), &["config", "--local", "user.email", "repair@example.test"]);
        fs::write(dir.path().join("fix.rs"), "fn ok() {}\n").unwrap();
        run_git(dir.path(), &["add", "fix.rs"]);
        run_git(dir.path(), &["commit", "--quiet", "--message", "the fix"]);
        let sha = git_stdout(dir.path(), &["rev-parse", "HEAD"]);
        (dir, sha)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        assert!(Command::new("git").current_dir(dir).args(args).status().unwrap().success(), "git {args:?} failed");
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn a_reachable_commit_records_both_rows_and_pushes_the_candidate_ref() {
        // Tripwire: a from-commit prepare that derived the pair but skipped the
        // push or either correspondence row would leave Verify unable to check
        // the commit out — the same hole the operator used to fill by hand.
        let (repo, sha) = repo_with_commit();
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let pusher = RecordingPush { pushed: Mutex::new(Vec::new()) };
        let workpiece = "issue-5032";

        let candidate =
            prepare_candidate(&store, &pusher, &bloom(), workpiece, CandidateSource::Commit(&sha), repo.path())
                .expect("a reachable commit prepares");

        let commit = BackendObjectId::from(aether_bloomery_github::GitObjectId::from_hex(&sha).unwrap());
        let tree_hex = git_stdout(repo.path(), &["rev-parse", &format!("{sha}^{{tree}}")]);
        let tree = BackendObjectId::from(aether_bloomery_github::GitObjectId::from_hex(&tree_hex).unwrap());

        assert_eq!(store.resolve_backend_object(&candidate.tree).unwrap().as_ref(), Some(&tree));
        assert_eq!(store.resolve_backend_object(&candidate.checkout).unwrap().as_ref(), Some(&commit));
        assert_eq!(
            pusher.pushed.lock().unwrap().as_slice(),
            &[(sha, candidate_ref_name(&bloom(), workpiece))],
            "the prepare must push the capture commit to the workpiece's candidate ref",
        );
    }

    #[test]
    fn an_unreachable_commit_is_refused_by_name() {
        // Tripwire: a missing or malformed revision must not mint a digest of
        // empty bytes or a last-writer-wins row. The refusal has to name the
        // commit, or the operator cannot tell which input failed.
        let (repo, _) = repo_with_commit();
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let missing = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

        let error = prepare_candidate(
            &store,
            &RecordingPush { pushed: Mutex::new(Vec::new()) },
            &bloom(),
            "issue-5032",
            CandidateSource::Commit(missing),
            repo.path(),
        )
        .expect_err("an unreachable commit must refuse");

        let message = error.to_string();
        assert!(
            matches!(error, PrepareError::Unreachable { .. }),
            "the refusal must be the unreachable-commit arm, got {error:?}"
        );
        assert!(message.contains(missing), "the refusal must name the commit: {message}");
        assert!(message.contains("not reachable"), "the refusal must name the failing precondition: {message}");
    }

    #[test]
    fn a_digest_collision_is_refused_rather_than_overwritten() {
        // Tripwire: correspondence is last-writer-wins. A prepare that called
        // `record` blindly would re-point a live digest at the operator's
        // object and silently break every lane still checking the old one out.
        let (repo, sha) = repo_with_commit();
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let tree_hex = git_stdout(repo.path(), &["rev-parse", &format!("{sha}^{{tree}}")]);
        let tree = BackendObjectId::from(aether_bloomery_github::GitObjectId::from_hex(&tree_hex).unwrap());
        let stranger = BackendObjectId::new(vec![0xee; 20]);
        let tree_digest = candidate_tree_digest(&tree);
        store.record(&tree_digest, &stranger).unwrap();

        let error = prepare_candidate(
            &store,
            &RecordingPush { pushed: Mutex::new(Vec::new()) },
            &bloom(),
            "issue-5032",
            CandidateSource::Commit(&sha),
            repo.path(),
        )
        .expect_err("a colliding digest must refuse");

        assert!(
            matches!(error, PrepareError::Collision { axis: "tree", .. }),
            "the refusal must name the tree axis, got {error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("already corresponds"), "the refusal must name the collision: {message}");
        assert_eq!(
            store.resolve_backend_object(&tree_digest).unwrap().as_ref(),
            Some(&stranger),
            "a refused prepare must leave the existing row in place",
        );
    }

    #[test]
    fn a_worktree_head_prepares_the_same_pair_as_its_commit() {
        // Tripwire: --from-worktree is HEAD-of-this-checkout, not a dirty-tree
        // recapture. Resolving a different commit than `--from-commit HEAD`
        // would hand Verify the wrong checkout.
        let (repo, sha) = repo_with_commit();
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let pusher = RecordingPush { pushed: Mutex::new(Vec::new()) };

        let from_worktree = prepare_candidate(
            &store,
            &pusher,
            &bloom(),
            "issue-5032",
            CandidateSource::Worktree(repo.path()),
            repo.path(),
        )
        .expect("a worktree of the coordinator repo prepares");
        let from_commit = {
            let store = SqliteCorrespondence::open(":memory:").unwrap();
            prepare_candidate(
                &store,
                &RecordingPush { pushed: Mutex::new(Vec::new()) },
                &bloom(),
                "issue-5032",
                CandidateSource::Commit(&sha),
                repo.path(),
            )
            .unwrap()
        };

        assert_eq!(from_worktree, from_commit, "HEAD of the worktree is the same commit the operator could have named");
    }

    #[test]
    fn a_push_failure_names_the_ref() {
        let (repo, sha) = repo_with_commit();
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let target = candidate_ref_name(&bloom(), "issue-5032");

        let error =
            prepare_candidate(&store, &FailingPush, &bloom(), "issue-5032", CandidateSource::Commit(&sha), repo.path())
                .expect_err("a refused push must surface");

        let message = error.to_string();
        assert!(message.contains(&target), "the refusal must name the candidate ref: {message}");
        assert!(message.contains("pushing"), "the refusal must name the failing precondition: {message}");
    }
}
