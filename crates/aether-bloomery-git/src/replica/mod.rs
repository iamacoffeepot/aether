//! One-way source replica: explicit allowlisted push from a local authority
//! to a configured GitHub URL (ADR-0199).
//!
//! Never `git push --mirror` — that would publish claim, attempt, candidate,
//! and checkpoint refs. Credentials stay on the caller: a bearer token is
//! passed in-process as an `http.extraHeader` and is never written to disk
//! or to a remote URL.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::mainline::MainlineRef;

mod allowlist;
#[cfg(test)]
mod tests;

pub use allowlist::{PublishedRefspec, published_refspecs};

/// Why a replica publish failed.
#[derive(Debug)]
pub enum ReplicaError {
    /// A transport or git fault that should stay queued for redrive.
    Transient(String),
    /// The replica refused a mainline force-push. Operator-visible: do not
    /// retry this request silently.
    ForceRejected(String),
}

impl std::fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(detail) => write!(f, "source replica push failed: {detail}"),
            Self::ForceRejected(detail) => {
                write!(f, "source replica force-push was rejected: {detail}")
            }
        }
    }
}

impl std::error::Error for ReplicaError {}

impl From<io::Error> for ReplicaError {
    fn from(error: io::Error) -> Self {
        Self::Transient(error.to_string())
    }
}

/// Publish allowlisted refs from a local authority to a git remote URL.
pub struct GitSourceReplica {
    authority: PathBuf,
    remote: String,
    mainline: MainlineRef,
    token: String,
}

impl GitSourceReplica {
    /// Push from `authority` to `remote`. `token` stays in this process and is
    /// applied as an HTTP header; it is never interpolated into `remote`.
    #[must_use]
    pub fn new(
        authority: impl Into<PathBuf>,
        remote: impl Into<String>,
        mainline: MainlineRef,
        token: impl Into<String>,
    ) -> Self {
        Self { authority: authority.into(), remote: remote.into(), mainline, token: token.into() }
    }

    /// The remote URL this replica pushes to (no credentials).
    #[must_use]
    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// The `git push` argv after `git -C <authority>`: never `--mirror`,
    /// force only via a `+` prefix on the mainline refspec.
    #[must_use]
    pub fn push_args(remote: &str, specs: &[PublishedRefspec]) -> Vec<String> {
        let mut args = vec!["push".to_owned(), "--porcelain".to_owned(), remote.to_owned()];
        args.extend(specs.iter().map(PublishedRefspec::as_arg));
        args
    }

    /// List fully-qualified refs in `authority`.
    ///
    /// # Errors
    /// `git for-each-ref` could not be spawned or exited non-zero.
    pub fn list_refs(authority: &Path) -> Result<Vec<String>, ReplicaError> {
        let output =
            Command::new("git").arg("-C").arg(authority).args(["for-each-ref", "--format=%(refname)"]).output()?;
        if !output.status.success() {
            return Err(ReplicaError::Transient(format!(
                "git for-each-ref: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    /// Push the allowlisted refs now.
    ///
    /// # Errors
    /// The remote was unreachable, git failed, or the replica rejected a
    /// mainline force-push.
    pub fn push(&self) -> Result<(), ReplicaError> {
        let refs = Self::list_refs(&self.authority)?;
        let specs = published_refspecs(&self.mainline, &refs);
        let args = Self::push_args(&self.remote, &specs);
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.authority).env("GIT_TERMINAL_PROMPT", "0");
        if !self.token.is_empty() {
            command.arg("-c").arg(format!("http.extraHeader=Authorization: Bearer {}", self.token));
        }
        let output = command.args(&args).output()?;
        classify_push(&output)
    }
}

/// The source-replica publish surface the host reactor drives.
pub trait SourceReplica: Send + Sync {
    /// Push the current allowlisted refs.
    ///
    /// # Errors
    /// Transient transport failure or a rejected mainline force-push.
    fn publish(&self) -> Result<(), ReplicaError>;
}

impl SourceReplica for GitSourceReplica {
    fn publish(&self) -> Result<(), ReplicaError> {
        self.push()
    }
}

fn classify_push(output: &Output) -> Result<(), ReplicaError> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = format!("{stderr}{stdout}");
    let lower = detail.to_ascii_lowercase();
    if is_force_rejection(&lower) {
        return Err(ReplicaError::ForceRejected(detail.trim().to_owned()));
    }
    Err(ReplicaError::Transient(detail.trim().to_owned()))
}

fn is_force_rejection(lower: &str) -> bool {
    lower.contains("protected branch")
        || lower.contains("hook declined")
        || lower.contains("remote rejected")
        || (lower.contains("rejected") && lower.contains("non-fast-forward"))
}
