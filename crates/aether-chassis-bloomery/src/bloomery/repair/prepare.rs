//! Turn a reachable commit into the candidate the repair door admits.
//!
//! The operator names a commit (or a worktree whose `HEAD` is one). This
//! module derives the tree and checkout digests the executor would have
//! minted, records both correspondence rows, and force-pushes the workpiece's
//! candidate ref so the verifying lane can check the commit out. Side effects
//! run before the journaled repair so a `Verify` dispatch that follows can
//! resolve the pair.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::Path;
use std::process::Command;

use aether_bloomery::{BackendObjectId, BloomId, CandidateRef, Correspondence, CorrespondenceError, Digest};
use aether_bloomery_github::{GitObjectId, candidate_ref_name};

use super::{candidate_tree_digest, capture_commit_digest};
use crate::bloomery::CandidatePush;

/// Where the operator is pointing: a commit the coordinator's repository can
/// already see, or a worktree whose `HEAD` must resolve to one.
#[derive(Clone, Copy, Debug)]
pub enum CandidateSource<'a> {
    /// A git revision (`sha`, `sha^{commit}`, a ref the coordinator repo has).
    Commit(&'a str),
    /// A worktree path; its `HEAD` is resolved, then treated as [`Self::Commit`].
    Worktree(&'a Path),
}

/// Why preparing a candidate from a commit failed. Every arm names the
/// precondition that did not hold, so the repair route can refuse with the
/// same sentence the operator needs.
#[derive(Debug)]
pub enum PrepareError {
    /// The named commit (or the worktree's `HEAD`) is not a reachable commit
    /// in the coordinator's repository.
    Unreachable {
        /// What the operator named.
        source: String,
        /// The git diagnostic, or why the worktree could not be read.
        detail: String,
    },
    /// A derived digest already maps to different backend bytes — recording
    /// would silently re-point a live correspondence.
    Collision {
        /// `"tree"` or `"checkout"`.
        axis: &'static str,
        /// The digest that already has a row.
        digest: Digest,
        /// The object that row already names.
        existing: BackendObjectId,
        /// The object this prepare wanted to record.
        requested: BackendObjectId,
    },
    /// The backend object already maps to a different digest — the reverse of
    /// [`Self::Collision`], refused for the same reason.
    ReverseCollision {
        /// `"tree"` or `"checkout"`.
        axis: &'static str,
        /// The object that already has a row.
        object: BackendObjectId,
        /// The digest that row already names.
        existing: Digest,
        /// The digest this prepare wanted to record.
        requested: Digest,
    },
    /// The correspondence store could not be read or written.
    Correspondence(CorrespondenceError),
    /// The candidate-ref push failed after the correspondence was recorded.
    Push {
        /// The ref the push targeted.
        target_ref: String,
        /// The pusher's diagnostic.
        detail: String,
    },
}

impl Display for PrepareError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { source, detail } => {
                write!(f, "commit `{source}` is not reachable from the coordinator's repository: {detail}")
            }
            Self::Collision { axis, digest, existing, requested } => write!(
                f,
                "derived {axis} digest {} already corresponds to {}, not {}",
                hex_bytes(digest.as_bytes()),
                hex_bytes(existing.as_bytes()),
                hex_bytes(requested.as_bytes()),
            ),
            Self::ReverseCollision { axis, object, existing, requested } => write!(
                f,
                "backend {axis} object {} already corresponds to digest {}, not {}",
                hex_bytes(object.as_bytes()),
                hex_bytes(existing.as_bytes()),
                hex_bytes(requested.as_bytes()),
            ),
            Self::Correspondence(error) => write!(f, "{error}"),
            Self::Push { target_ref, detail } => {
                write!(f, "pushing the candidate to `{target_ref}` failed: {detail}")
            }
        }
    }
}

impl Error for PrepareError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Correspondence(error) => Some(error),
            Self::Unreachable { .. } | Self::Collision { .. } | Self::ReverseCollision { .. } | Self::Push { .. } => {
                None
            }
        }
    }
}

/// Derive the candidate pair from `source`, record both correspondence rows,
/// and force-push the workpiece's candidate ref.
///
/// `repo` is the coordinator's clone — the same cwd the production candidate
/// pusher shells `git push` from. A worktree source is resolved there so a foreign clone's
/// `HEAD` is refused rather than pushed as an object this process cannot see.
///
/// # Errors
/// The commit is unreachable, a derived digest collides with a different
/// object, the correspondence store faulted, or the push failed.
pub fn prepare_candidate(
    correspondence: &dyn Correspondence,
    pusher: &dyn CandidatePush,
    bloom: &BloomId,
    workpiece: &str,
    source: CandidateSource<'_>,
    repo: &Path,
) -> Result<CandidateRef, PrepareError> {
    let commit_hex = match source {
        CandidateSource::Commit(revision) => revision.trim().to_owned(),
        CandidateSource::Worktree(path) => worktree_head(path)?,
    };
    let objects = resolve_commit(repo, &commit_hex, &source_label(source, &commit_hex))?;
    let candidate =
        CandidateRef { tree: candidate_tree_digest(&objects.tree), checkout: capture_commit_digest(&objects.commit) };
    record_unique(correspondence, "tree", &candidate.tree, &objects.tree)?;
    record_unique(correspondence, "checkout", &candidate.checkout, &objects.commit)?;

    let target_ref = candidate_ref_name(bloom, workpiece);
    pusher.push(&objects.commit_hex, &target_ref).map_err(|detail| PrepareError::Push { target_ref, detail })?;
    Ok(candidate)
}

struct ResolvedObjects {
    commit_hex: String,
    commit: BackendObjectId,
    tree: BackendObjectId,
}

fn source_label(source: CandidateSource<'_>, commit_hex: &str) -> String {
    match source {
        CandidateSource::Commit(_) => commit_hex.to_owned(),
        CandidateSource::Worktree(path) => format!("{commit_hex} (HEAD of {})", path.display()),
    }
}

fn worktree_head(path: &Path) -> Result<String, PrepareError> {
    git_in(path, &["rev-parse", "--verify", "--end-of-options", "HEAD"]).map_err(|detail| PrepareError::Unreachable {
        source: path.display().to_string(),
        detail: format!("could not read HEAD: {detail}"),
    })
}

fn resolve_commit(repo: &Path, revision: &str, source: &str) -> Result<ResolvedObjects, PrepareError> {
    if revision.is_empty() {
        return Err(PrepareError::Unreachable { source: source.to_owned(), detail: "no commit was named".to_owned() });
    }
    let commit_peel = format!("{revision}^{{commit}}");
    let commit_hex = git_in(repo, &["rev-parse", "--verify", "--end-of-options", &commit_peel])
        .map_err(|detail| PrepareError::Unreachable { source: source.to_owned(), detail })?;
    let tree_peel = format!("{commit_hex}^{{tree}}");
    let tree_hex = git_in(repo, &["rev-parse", "--verify", "--end-of-options", &tree_peel])
        .map_err(|detail| PrepareError::Unreachable { source: source.to_owned(), detail })?;
    let commit = object_id(&commit_hex, source, "commit")?;
    let tree = object_id(&tree_hex, source, "tree")?;
    Ok(ResolvedObjects { commit_hex, commit, tree })
}

fn object_id(hex: &str, source: &str, kind: &str) -> Result<BackendObjectId, PrepareError> {
    GitObjectId::from_hex(hex).map(BackendObjectId::from).ok_or_else(|| PrepareError::Unreachable {
        source: source.to_owned(),
        detail: format!("resolved {kind} `{hex}` is not a git object id"),
    })
}

fn record_unique(
    correspondence: &dyn Correspondence,
    axis: &'static str,
    digest: &Digest,
    object: &BackendObjectId,
) -> Result<(), PrepareError> {
    if let Some(existing) = correspondence.resolve_backend_object(digest).map_err(PrepareError::Correspondence)?
        && existing != *object
    {
        return Err(PrepareError::Collision { axis, digest: *digest, existing, requested: object.clone() });
    }
    if let Some(existing) = correspondence.resolve_digest(object).map_err(PrepareError::Correspondence)?
        && existing != *digest
    {
        return Err(PrepareError::ReverseCollision { axis, object: object.clone(), existing, requested: *digest });
    }
    correspondence.record(digest, object).map_err(PrepareError::Correspondence)
}

fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git").current_dir(dir).args(args).output().map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("git {args:?} failed")
        } else {
            stderr
        })
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    aether_bloomery::encode_hex(bytes)
}
