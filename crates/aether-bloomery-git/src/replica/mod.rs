//! One-way source replica: explicit allowlisted push from a local authority
//! to a configured GitHub URL (ADR-0199).
//!
//! Never `git push --mirror` — that would publish claim, attempt, candidate,
//! and checkpoint refs. Credentials stay on the caller: a token is passed
//! in-process as an `http.extraHeader` (`Authorization: Basic` of
//! `x-access-token:<token>`) and is never written to disk or to a remote URL.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::mainline::MainlineRef;

mod allowlist;
mod classify;
#[cfg(test)]
mod tests;

pub use allowlist::{PublishedRefspec, published_refspecs};

/// Identical [`ReplicaError::Transient`] failures beyond this count escalate
/// to [`ReplicaError::Deterministic`] rather than staying queued at warn.
pub const DEFAULT_TRANSIENT_REDRIVE_LIMIT: usize = 5;

/// Why a replica publish failed.
#[derive(Debug)]
pub enum ReplicaError {
    /// A transport or git fault that should stay queued for redrive.
    Transient(String),
    /// The replica refused a mainline force-push. Operator-visible: do not
    /// retry this request silently.
    ForceRejected(String),
    /// A deterministic refusal — auth, absent mainline, unknown remote,
    /// missing binary, a non-mainline ref rejection, or a transient failure
    /// that has redriven past [`DEFAULT_TRANSIENT_REDRIVE_LIMIT`].
    /// Operator-visible: do not retry this request silently.
    Deterministic(String),
}

impl fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient(detail) => write!(f, "source replica push failed: {detail}"),
            Self::ForceRejected(detail) => {
                write!(f, "source replica force-push was rejected: {detail}")
            }
            Self::Deterministic(detail) => write!(f, "source replica push refused: {detail}"),
        }
    }
}

impl Error for ReplicaError {}

impl From<io::Error> for ReplicaError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::Deterministic(format!("git binary not found: {error}"))
        } else {
            Self::Transient(error.to_string())
        }
    }
}

struct TransientRedrive {
    detail: String,
    count: usize,
}

/// Publish allowlisted refs from a local authority to a git remote URL.
pub struct GitSourceReplica {
    authority: PathBuf,
    remote: String,
    mainline: MainlineRef,
    token: String,
    transient_redrive_limit: usize,
    transient_redrives: Mutex<Option<TransientRedrive>>,
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
        Self {
            authority: authority.into(),
            remote: remote.into(),
            mainline,
            token: token.into(),
            transient_redrive_limit: DEFAULT_TRANSIENT_REDRIVE_LIMIT,
            transient_redrives: Mutex::new(None),
        }
    }

    /// Bound identical [`ReplicaError::Transient`] redrives before escalation
    /// to [`ReplicaError::Deterministic`].
    #[must_use]
    pub fn with_transient_redrive_limit(mut self, limit: usize) -> Self {
        self.transient_redrive_limit = limit;
        self
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
    /// The remote was unreachable, git failed, the replica rejected a
    /// mainline force-push, or a deterministic refusal (auth, absent
    /// mainline, unknown remote).
    pub fn push(&self) -> Result<(), ReplicaError> {
        let refs = Self::list_refs(&self.authority)?;
        let mainline = self.mainline.to_string();
        if !refs.iter().any(|name| name == &mainline) {
            return Err(ReplicaError::Deterministic(format!("authority has no mainline ref {mainline}")));
        }
        let specs = published_refspecs(&self.mainline, &refs);
        self.record_push(classify::classify_push(&self.git_push_command(&specs).output()?, &mainline))
    }

    fn record_push(&self, result: Result<(), ReplicaError>) -> Result<(), ReplicaError> {
        let mut slot = self.transient_redrives.lock().expect("replica redrive mutex");
        match result {
            Ok(()) => {
                *slot = None;
                Ok(())
            }
            Err(ReplicaError::Transient(detail)) => {
                let count = match slot.as_ref() {
                    Some(prior) if prior.detail == detail => prior.count.saturating_add(1),
                    _ => 1,
                };
                *slot = Some(TransientRedrive { detail: detail.clone(), count });
                if count > self.transient_redrive_limit {
                    Err(ReplicaError::Deterministic(format!("transient failure repeated {count} times: {detail}")))
                } else {
                    Err(ReplicaError::Transient(detail))
                }
            }
            Err(other) => {
                *slot = None;
                Err(other)
            }
        }
    }

    fn git_push_command(&self, specs: &[PublishedRefspec]) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.authority).env("GIT_TERMINAL_PROMPT", "0");
        if !self.token.is_empty() {
            command.arg("-c").arg(authorization_extra_header(&self.token));
        }
        command.args(Self::push_args(&self.remote, specs));
        command
    }
}

/// GitHub's git-over-HTTPS header. `Authorization: Bearer` is rejected for
/// `gh`-keyring OAuth tokens; Basic of `x-access-token:<token>` is accepted.
fn authorization_extra_header(token: &str) -> String {
    format!("http.extraHeader=Authorization: Basic {}", encode_std_base64(format!("x-access-token:{token}").as_bytes()))
}

fn encode_std_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (chunk.get(1).copied().map_or(0, u32::from) << 8)
            | chunk.get(2).copied().map_or(0, u32::from);
        out.push(char::from(TABLE[((n >> 18) & 0x3f) as usize]));
        out.push(char::from(TABLE[((n >> 12) & 0x3f) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[((n >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[(n & 0x3f) as usize])
        } else {
            '='
        });
    }
    out
}

/// The source-replica publish surface the host reactor drives.
pub trait SourceReplica: Send + Sync {
    /// Push the current allowlisted refs.
    ///
    /// # Errors
    /// Transient transport failure, a rejected mainline force-push, or a
    /// deterministic refusal.
    fn publish(&self) -> Result<(), ReplicaError>;
}

impl SourceReplica for GitSourceReplica {
    fn publish(&self) -> Result<(), ReplicaError> {
        self.push()
    }
}
