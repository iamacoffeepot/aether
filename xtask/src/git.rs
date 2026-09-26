//! One place that spawns `git` and classifies what came back.
//!
//! The symbol inventory, the verify lane's symbol pass, and `import-commit`
//! all read history through this module, so load-bearing flags cannot drift
//! per call site and every failed spawn renders the same way.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

/// Why a git spawn or its output could not be used.
#[derive(Debug)]
pub enum GitCommandError {
    /// The process could not be started.
    Spawn {
        /// The argv after `git` (and after `-C <repo>` when one was set).
        args: String,
        /// The `-C` repository, when the spawn was repo-scoped.
        repo: Option<String>,
        /// The IO fault from `Command::output`.
        source: io::Error,
    },
    /// git ran and exited non-zero.
    Failed {
        /// The argv after `git`.
        args: String,
        /// Trimmed stderr git printed.
        stderr: String,
    },
}

impl fmt::Display for GitCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { args, repo: Some(repo), source } => write!(f, "git {args} in {repo}: {source}"),
            Self::Spawn { args, repo: None, source } => write!(f, "git {args}: {source}"),
            Self::Failed { args, stderr } if stderr.is_empty() => write!(f, "git {args} failed"),
            Self::Failed { args, stderr } => write!(f, "git {args}: {stderr}"),
        }
    }
}

impl Error for GitCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Failed { .. } => None,
        }
    }
}

/// Trimmed lossy UTF-8 of `bytes`.
#[must_use]
pub fn trim_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

/// Run `git -C repo args…` and return the raw output.
///
/// # Errors
/// The process could not be spawned.
pub fn run(repo: &Path, args: &[&str]) -> Result<Output, GitCommandError> {
    Command::new("git").arg("-C").arg(repo).args(args).output().map_err(|source| GitCommandError::Spawn {
        args: format!("{args:?}"),
        repo: Some(repo.display().to_string()),
        source,
    })
}

/// Run `git -C repo args…` and return trimmed stdout on success.
///
/// # Errors
/// Spawn failed or the command exited non-zero.
pub fn run_ok(repo: &Path, args: &[&str]) -> Result<String, GitCommandError> {
    let output = run(repo, args)?;
    if !output.status.success() {
        return Err(GitCommandError::Failed { args: format!("{args:?}"), stderr: trim_bytes(&output.stderr) });
    }
    Ok(trim_bytes(&output.stdout))
}

/// Start `git -C repo args…` with piped stdin and stdout, for a long-lived
/// batch reader such as `cat-file --batch`. stderr stays the terminal's, so
/// git's own diagnostic reaches the operator.
///
/// # Errors
/// The process could not be spawned.
pub fn spawn_piped(repo: &Path, args: &[&str]) -> Result<Child, GitCommandError> {
    Command::new("git").arg("-C").arg(repo).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(
        |source| GitCommandError::Spawn { args: format!("{args:?}"), repo: Some(repo.display().to_string()), source },
    )
}
