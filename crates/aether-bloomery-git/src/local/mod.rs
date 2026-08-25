//! Fleet-local [`GitDataApi`] over an absolute bare-repository path (ADR-0199).
//!
//! Speaks the installed `git` binary — no `git2`, no `gix`. Command execution
//! and error classification live in one helper so every verb classifies the
//! same way.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::client::{GitCommit, GitDataApi, GitDataError, GitRef, MergeResult, RefTxnOp, strip_heads};
use crate::command;

#[cfg(test)]
mod tests;

/// The checked-in merge driver, embedded so the script that ships in the binary
/// is the same one review reads. `include_str!` registers it as a build input,
/// so editing the script rebuilds this crate.
const MERGE_DRIVER_SOURCE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/merge-sorted-reexports.py"));

/// The file name the driver is materialized under inside the git directory.
const MERGE_DRIVER_SCRIPT: &str = "aether-merge-sorted-reexports.py";

/// The driver's `merge.<name>.name` — what git prints when it reports which
/// driver resolved (or declined) a path.
const MERGE_DRIVER_NAME: &str = "sorted one-name-per-line re-export lists";

/// The interpreter the driver is invoked through. Named rather than relying on
/// the script's own shebang because the git directory it is written into is not
/// guaranteed to be on an executable mount.
const PYTHON: &str = "python3";

/// A [`GitDataApi`] backed by `git` against one absolute repository path.
#[derive(Clone, Debug)]
pub struct LocalGitData {
    repo: PathBuf,
    zero_oid: String,
}

impl LocalGitData {
    /// Open `repo` as the local git-data authority.
    ///
    /// `repo` must be an absolute filesystem path (no `file://` prefix). Boot
    /// asserts the installed git is at least [`crate::command::MIN_GIT`] and names
    /// the version it found when it is not.
    ///
    /// # Errors
    /// The path is relative, the directory is not a git repository, git is
    /// missing or too old, or the empty tree could not be materialized.
    pub fn open(repo: impl Into<PathBuf>) -> Result<Self, GitDataError> {
        let repo = repo.into();
        if !repo.is_absolute() {
            return Err(GitDataError::Command(format!(
                "local git-data repository path must be absolute (no file:// prefix); got {}",
                repo.display()
            )));
        }
        command::require_min(command::detect_version()?)?;
        command::run_ok(&repo, &["rev-parse", "--absolute-git-dir"])
            .map_err(|error| GitDataError::Command(format!("{} is not a git repository: {error}", repo.display())))?;
        let format = command::run_ok(&repo, &["rev-parse", "--show-object-format"]).unwrap_or_else(|_| "sha1".into());
        let zero_oid = if format == "sha256" {
            "0".repeat(64)
        } else {
            "0".repeat(40)
        };
        let hashed = command::run_stdin(&repo, &["hash-object", "-t", "tree", "-w", "--stdin"], "")?;
        if !hashed.status.success() {
            return Err(GitDataError::Command(format!(
                "materializing the empty tree in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&hashed.stderr).trim()
            )));
        }
        let local = Self { repo, zero_oid };
        local.install_merge_driver()?;
        Ok(local)
    }

    /// Materialize the sorted-re-export merge driver and point this repository's
    /// config at it.
    ///
    /// Done at open rather than in a separate setup step so a fresh deployment
    /// has the driver without anyone remembering to install it, and the script
    /// is written into the git directory rather than read from a checkout
    /// because the fleet repository is bare — there is no working tree beside it
    /// holding `scripts/`. The bytes are the checked-in script, embedded at
    /// compile time, so the reviewable copy and the running copy cannot drift.
    ///
    /// Which paths the driver applies to is *not* decided here: that is
    /// `.gitattributes` in the merged trees, which a candidate can edit. Naming
    /// the driver in config and selecting it per path in the tree is git's own
    /// split, and it is why the driver is written to refuse anything that is not
    /// a sorted `pub use` insertion rather than to trust where it was pointed.
    fn install_merge_driver(&self) -> Result<(), GitDataError> {
        let git_dir = PathBuf::from(command::run_ok(&self.repo, &["rev-parse", "--absolute-git-dir"])?);
        let script = git_dir.join(MERGE_DRIVER_SCRIPT);
        fs::write(&script, MERGE_DRIVER_SOURCE)
            .map_err(|error| GitDataError::Command(format!("writing {}: {error}", script.display())))?;

        let driver = format!("{} {} %O %A %B", PYTHON, script.display());
        command::run_ok(&self.repo, &["config", "merge.sorted-reexports.name", MERGE_DRIVER_NAME])?;
        command::run_ok(&self.repo, &["config", "merge.sorted-reexports.driver", &driver])?;
        Ok(())
    }

    /// The absolute repository path this backend addresses.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    fn qualified(name: &str) -> String {
        if name.starts_with("refs/") {
            name.to_owned()
        } else {
            format!("refs/{name}")
        }
    }

    fn object_exists(&self, sha: &str) -> bool {
        command::run(&self.repo, &["cat-file", "-e", sha]).is_ok_and(|output| output.status.success())
    }

    fn resolve_commit(&self, name: &str) -> Result<String, GitDataError> {
        let short = strip_heads(name);
        if let Some(git_ref) = self.get_ref(&format!("heads/{short}"))? {
            return Ok(git_ref.sha);
        }
        if self.object_exists(name) {
            return Ok(name.to_owned());
        }
        Err(GitDataError::MissingObject(format!("no commit or ref {name}")))
    }

    fn conflicted_paths(&self, base: &str, head: &str) -> Vec<String> {
        command::conflicted_paths(&self.repo, base, head)
    }

    fn conflict_patch(&self, base: &str, head: &str) -> String {
        command::run_ok(&self.repo, &["diff", base, head]).unwrap_or_default()
    }
}

impl GitDataApi for LocalGitData {
    fn get_ref(&self, name: &str) -> Result<Option<GitRef>, GitDataError> {
        let qualified = Self::qualified(name);
        let output = command::run(&self.repo, &["for-each-ref", "--format=%(objectname)", &qualified])?;
        if !output.status.success() {
            return Err(GitDataError::Command(format!(
                "git for-each-ref {qualified}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if sha.is_empty() {
            return Ok(None);
        }
        Ok(Some(GitRef { name: name.to_owned(), sha }))
    }

    fn create_ref(&self, name: &str, sha: &str) -> Result<GitRef, GitDataError> {
        let qualified = Self::qualified(name);
        let output = command::run(&self.repo, &["update-ref", &qualified, sha, &self.zero_oid])?;
        if !output.status.success() {
            return Err(command::classify_update(&output, name));
        }
        Ok(GitRef { name: name.to_owned(), sha: sha.to_owned() })
    }

    fn update_ref(&self, name: &str, sha: &str, force: bool) -> Result<GitRef, GitDataError> {
        if force {
            let qualified = Self::qualified(name);
            let output = command::run(&self.repo, &["update-ref", &qualified, sha])?;
            if !output.status.success() {
                return Err(command::classify_update(&output, name));
            }
            return Ok(GitRef { name: name.to_owned(), sha: sha.to_owned() });
        }
        let current = self.get_ref(name)?.ok_or_else(|| GitDataError::MissingObject(format!("no ref {name}")))?;
        self.compare_and_swap_ref(name, sha, &current.sha)
    }

    fn delete_ref(&self, name: &str) -> Result<(), GitDataError> {
        let qualified = Self::qualified(name);
        let output = command::run(&self.repo, &["update-ref", "-d", &qualified])?;
        if output.status.success() {
            return Ok(());
        }
        Err(command::classify_update(&output, name))
    }

    fn list_matching_refs(&self, prefix: &str) -> Result<Vec<GitRef>, GitDataError> {
        let pattern = Self::qualified(prefix);
        let stdout = command::run_ok(&self.repo, &["for-each-ref", "--format=%(refname) %(objectname)", &pattern])?;
        let mut refs = Vec::new();
        for line in stdout.lines() {
            let Some((full, sha)) = line.rsplit_once(' ') else {
                continue;
            };
            let name = full.strip_prefix("refs/").unwrap_or(full);
            refs.push(GitRef { name: name.to_owned(), sha: sha.to_owned() });
        }
        Ok(refs)
    }

    fn get_commit(&self, sha: &str) -> Result<GitCommit, GitDataError> {
        command::read_commit(&self.repo, sha)
    }

    fn create_commit(&self, message: &str, tree: &str, parents: &[String]) -> Result<GitCommit, GitDataError> {
        let sha = command::commit_tree(&self.repo, message, tree, parents)?;
        Ok(GitCommit { sha, tree: tree.to_owned(), message: message.to_owned() })
    }

    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Result<bool, GitDataError> {
        command::is_ancestor(&self.repo, ancestor, commit)
    }

    fn merge(&self, base: &str, head: &str, message: &str) -> Result<MergeResult, GitDataError> {
        let base_sha = self.resolve_commit(base)?;
        let head_sha = self.resolve_commit(head)?;
        if self.is_ancestor(&head_sha, &base_sha)? {
            return Ok(MergeResult::AlreadyUpToDate);
        }

        // `merge-tree` merges in memory against a bare repository, so it reads
        // no `.gitattributes` from an index or a worktree and finds none in the
        // trees it is merging. Without an attribute source the `sorted-reexports`
        // driver named there never runs and a determined re-export fold
        // conflicts anyway. The base side is that source: the attributes that
        // decide a merge are the ones already on the branch being merged into,
        // never the ones the incoming candidate ships with itself. `MIN_GIT` is
        // the floor that guarantees `attr.tree` is honoured; an older git
        // accepts the `-c` key and ignores it.
        let attributes = format!("attr.tree={base_sha}");
        let output =
            command::run(&self.repo, &["-c", &attributes, "merge-tree", "--write-tree", &base_sha, &head_sha])?;
        match output.status.code() {
            Some(0) => {
                let tree = String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or_default().trim().to_owned();
                let commit = self.create_commit(message, &tree, &[base_sha.clone(), head_sha])?;
                let base_ref = format!("heads/{}", strip_heads(base));
                self.compare_and_swap_ref(&base_ref, &commit.sha, &base_sha)?;
                Ok(MergeResult::Merged(commit))
            }
            Some(1) => Ok(MergeResult::Conflict {
                detail: format!("merge conflict ({head} into {base})"),
                paths: self.conflicted_paths(&base_sha, &head_sha),
                patch: self.conflict_patch(&base_sha, &head_sha),
            }),
            _ => Err(GitDataError::Command(format!(
                "git merge-tree {base_sha} {head_sha}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    fn compare_and_swap_ref(&self, name: &str, sha: &str, expected: &str) -> Result<GitRef, GitDataError> {
        let qualified = Self::qualified(name);
        let output = command::run(&self.repo, &["update-ref", &qualified, sha, expected])?;
        if !output.status.success() {
            return Err(command::classify_update(&output, name));
        }
        Ok(GitRef { name: name.to_owned(), sha: sha.to_owned() })
    }

    fn transact_refs(&self, ops: &[RefTxnOp]) -> Result<(), GitDataError> {
        if ops.is_empty() {
            return Ok(());
        }
        let mut stdin = String::new();
        for op in ops {
            match op {
                RefTxnOp::Create { name, sha } => {
                    let _ = writeln!(stdin, "create {} {sha}", Self::qualified(name));
                }
                RefTxnOp::Update { name, sha, expected } => {
                    let _ = writeln!(stdin, "update {} {sha} {expected}", Self::qualified(name));
                }
                RefTxnOp::Delete { name, expected } => {
                    let _ = writeln!(stdin, "delete {} {expected}", Self::qualified(name));
                }
            }
        }
        let output = command::run_stdin(&self.repo, &["update-ref", "--stdin"], &stdin)?;
        if output.status.success() {
            return Ok(());
        }
        Err(command::classify_update(&output, "transaction"))
    }
}
