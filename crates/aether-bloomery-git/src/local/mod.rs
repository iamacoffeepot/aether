//! Fleet-local [`GitDataApi`] over an absolute bare-repository path (ADR-0199).
//!
//! Speaks the installed `git` binary — no `git2`, no `gix`. Command execution
//! and error classification live in one helper so every verb classifies the
//! same way.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::client::{GitCommit, GitDataApi, GitDataError, GitRef, MergeResult, RefTxnOp, strip_heads};

mod command;

#[cfg(test)]
mod tests;

/// The identity every locally minted commit carries. Pinned so a retry of
/// `create_commit` with the same `(message, tree, parents)` hashes to the same
/// object — `GitSource::integrate` recovers from a fault between commit and
/// ref update only because of that.
const BLOOMERY_IDENTITY: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "bloomery"),
    ("GIT_AUTHOR_EMAIL", "bloomery@aether.invalid"),
    ("GIT_AUTHOR_DATE", "@0 +0000"),
    ("GIT_COMMITTER_NAME", "bloomery"),
    ("GIT_COMMITTER_EMAIL", "bloomery@aether.invalid"),
    ("GIT_COMMITTER_DATE", "@0 +0000"),
];

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
    /// asserts the installed git is at least 2.38 and names the version it
    /// found when it is not.
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
        Ok(Self { repo, zero_oid })
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
        let Ok(output) = command::run(&self.repo, &["merge-tree", "--write-tree", "--name-only", base, head]) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !is_git_oid(line))
            .map(ToOwned::to_owned)
            .collect()
    }

    fn conflict_patch(&self, base: &str, head: &str) -> String {
        command::run_ok(&self.repo, &["diff", base, head]).unwrap_or_default()
    }
}

fn is_git_oid(line: &str) -> bool {
    (line.len() == 40 || line.len() == 64) && line.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_commit(sha: &str, body: &str) -> Result<GitCommit, GitDataError> {
    let mut tree = None;
    let mut message_start = None;
    for (index, line) in body.lines().enumerate() {
        if let Some(value) = line.strip_prefix("tree ") {
            tree = Some(value.trim().to_owned());
        } else if line.is_empty() {
            message_start = Some(index + 1);
            break;
        }
    }
    let tree = tree.ok_or_else(|| GitDataError::Command(format!("commit {sha} has no tree header")))?;
    let message =
        message_start.map_or_else(String::new, |start| body.lines().skip(start).collect::<Vec<_>>().join("\n"));
    Ok(GitCommit { sha: sha.to_owned(), tree, message })
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
        if output.status.success() || command::is_absent_delete(&String::from_utf8_lossy(&output.stderr)) {
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
        let kind = command::run(&self.repo, &["cat-file", "-t", sha])?;
        if !kind.status.success() {
            return Err(GitDataError::MissingObject(format!("no commit {sha}")));
        }
        if String::from_utf8_lossy(&kind.stdout).trim() != "commit" {
            return Err(GitDataError::MissingObject(format!("no commit {sha}")));
        }
        let body = command::run_ok(&self.repo, &["cat-file", "commit", sha])
            .map_err(|_| GitDataError::MissingObject(format!("no commit {sha}")))?;
        parse_commit(sha, &body)
    }

    fn create_commit(&self, message: &str, tree: &str, parents: &[String]) -> Result<GitCommit, GitDataError> {
        let mut args = vec!["commit-tree".to_owned(), tree.to_owned()];
        for parent in parents {
            args.push("-p".to_owned());
            args.push(parent.clone());
        }
        args.push("-m".to_owned());
        args.push(message.to_owned());
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = command::run_env(&self.repo, &borrowed, &BLOOMERY_IDENTITY)?;
        if !output.status.success() {
            return Err(GitDataError::Command(format!(
                "git commit-tree {tree}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(GitCommit { sha, tree: tree.to_owned(), message: message.to_owned() })
    }

    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Result<bool, GitDataError> {
        if ancestor == commit {
            return Ok(true);
        }
        let output = command::run(&self.repo, &["merge-base", "--is-ancestor", ancestor, commit])?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) if self.object_exists(ancestor) && self.object_exists(commit) => Ok(false),
            Some(1) => Err(GitDataError::MissingObject(format!("missing ancestor {ancestor} or commit {commit}"))),
            _ => Err(GitDataError::Command(format!(
                "git merge-base --is-ancestor {ancestor} {commit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    fn merge(&self, base: &str, head: &str, message: &str) -> Result<MergeResult, GitDataError> {
        let base_sha = self.resolve_commit(base)?;
        let head_sha = self.resolve_commit(head)?;
        if self.is_ancestor(&head_sha, &base_sha)? {
            return Ok(MergeResult::AlreadyUpToDate);
        }

        let output = command::run(&self.repo, &["merge-tree", "--write-tree", &base_sha, &head_sha])?;
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
