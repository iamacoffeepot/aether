//! The local-process executor-port fault type.

use std::error::Error;
use std::fmt;
use std::io;

use aether_bloomery::{CorrespondenceError, Nonce};

/// A local-process executor-port fault. Its own type because the port needs an
/// arm the value vocabulary does not carry — a message asked to act on a run
/// that does not resolve for its nonce — alongside the worktree / spawn / io /
/// evidence faults the local lane produces.
#[derive(Debug)]
pub enum LocalExecutorError {
    /// Materializing the order's checkout into the scratch worktree failed (the
    /// `git worktree add` shell-out, carrying its stderr tail).
    Worktree(String),
    /// Spawning the `cargo xtask transform` child failed.
    Spawn(io::Error),
    /// A filesystem operation (create the run dir, kill/reap the child) failed.
    Io(io::Error),
    /// The run's `evidence.json` could not be read (the run wrote none, or the
    /// read faulted).
    Evidence(String),
    /// A `cancel` / `stream_evidence` resolved no tracked run for the nonce — the
    /// order was never submitted to this backend, or was already consumed.
    /// (`inspect` reports the same condition as the clean
    /// [`ExecutionStatus::Unknown`](aether_bloomery::ExecutionStatus::Unknown).)
    NoRunForNonce(Nonce),
    /// The order's checkout digest resolved no real git object through the
    /// correspondence store (ADR-0150) — the sealed source was never materialized
    /// or its correspondence never seeded, so the backend refuses cleanly rather
    /// than `git worktree add`-ing a target git cannot resolve.
    UnresolvedCheckout(Nonce),
    /// The order named a diff base (the range the candidate is judged over) that
    /// resolved no real git object. Refused rather than dropped: a review lane
    /// handed no base falls back to the working-tree contract, and against an
    /// already-committed candidate that diff is empty — which reads as "nothing
    /// to review" rather than "the base did not resolve" (#4723).
    UnresolvedDiffBase(Nonce),
    /// The correspondence store itself faulted while resolving the checkout.
    Correspondence(CorrespondenceError),
}

impl fmt::Display for LocalExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Worktree(detail) => write!(f, "local executor backend: worktree checkout failed: {detail}"),
            Self::Spawn(error) => write!(f, "local executor backend: spawn transform failed: {error}"),
            Self::Io(error) => write!(f, "local executor backend: {error}"),
            Self::Evidence(detail) => write!(f, "local executor backend: evidence read failed: {detail}"),
            Self::NoRunForNonce(nonce) => {
                write!(f, "local executor backend: no run resolves for nonce `{}`", nonce.0)
            }
            Self::UnresolvedCheckout(nonce) => {
                write!(
                    f,
                    "local executor backend: no git-object correspondence for the checkout of nonce `{}`",
                    nonce.0
                )
            }
            Self::UnresolvedDiffBase(nonce) => {
                write!(
                    f,
                    "local executor backend: no git-object correspondence for the diff base of nonce `{}`",
                    nonce.0
                )
            }
            Self::Correspondence(error) => write!(f, "local executor backend: {error}"),
        }
    }
}

impl Error for LocalExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Io(error) => Some(error),
            Self::Correspondence(error) => Some(error),
            Self::Worktree(_)
            | Self::Evidence(_)
            | Self::NoRunForNonce(_)
            | Self::UnresolvedCheckout(_)
            | Self::UnresolvedDiffBase(_) => None,
        }
    }
}

impl From<CorrespondenceError> for LocalExecutorError {
    fn from(error: CorrespondenceError) -> Self {
        Self::Correspondence(error)
    }
}
