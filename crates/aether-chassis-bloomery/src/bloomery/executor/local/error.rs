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
    /// A `stream_evidence` resolved no tracked run for the nonce — the order was
    /// never submitted to this backend, or was already consumed. (`inspect`
    /// reports the same condition as the clean
    /// [`ExecutionStatus::Unknown`](aether_bloomery::ExecutionStatus::Unknown),
    /// and `cancel` as a clean success — it is idempotent per ADR-0177, so an
    /// absent run is already cancelled.)
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
    /// The cancel could not terminate its child. Distinct from a clean
    /// `Ok(())` so a caller cannot treat an unowned or still-alive child as
    /// gone (issue #4999). The string names why: no recorded identity, a
    /// mismatched identity, or a process group that stayed up after the
    /// signal.
    Unterminated(String),
    /// The tree the dispatch's checkout materialized is not the candidate tree
    /// the order binds its returned evidence to (ADR-0152).
    ///
    /// A machinery fault, never a verdict: the lane would have judged whatever
    /// the checkout happened to carry — a splice built from an earlier lap, a
    /// re-pointed correspondence row — and the gate's answer would have been
    /// filed against a candidate no gate ever saw. On 2026-08-26 that filed a
    /// repair lap's fix as unjudged, failed the same two verifiers a second
    /// time over the identical content, and wedged the member on the
    /// repeated-verifiers ceiling as if the model had produced the failure.
    ///
    /// Refused before the child is spawned, so no lap is paid for; the backend
    /// records it as a host fault (ADR-0195) rather than returning it to the
    /// drain, because the identical order would materialize the identical
    /// stale tree on every re-drive.
    StaleCandidateCheckout {
        /// The dispatch's idempotency nonce.
        nonce: String,
        /// The git tree object the order's candidate digest resolves to.
        expected: String,
        /// The tree the materialized checkout actually carries.
        observed: String,
    },
    /// The lane-host tool kit is incomplete, so this dispatch was refused
    /// before a child was spawned (#5035). Transient: installing the missing
    /// tools clears the next re-drain, and the member stays queued rather
    /// than accruing a failed attempt. The string is [`KitReport::render_refusal`].
    ///
    /// [`KitReport::render_refusal`]: crate::bloomery::KitReport::render_refusal
    MissingKit(String),
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
            Self::StaleCandidateCheckout { nonce, expected, observed } => write!(
                f,
                "local executor backend: the checkout for nonce `{nonce}` carries tree `{observed}`, \
                 not the candidate tree `{expected}` the order binds its evidence to",
            ),
            Self::Correspondence(error) => write!(f, "local executor backend: {error}"),
            Self::Unterminated(detail) => {
                write!(f, "local executor backend: could not terminate the lane child: {detail}")
            }
            Self::MissingKit(detail) => write!(f, "local executor backend: {detail}"),
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
            | Self::UnresolvedDiffBase(_)
            | Self::StaleCandidateCheckout { .. }
            | Self::Unterminated(_)
            | Self::MissingKit(_) => None,
        }
    }
}

impl From<CorrespondenceError> for LocalExecutorError {
    fn from(error: CorrespondenceError) -> Self {
        Self::Correspondence(error)
    }
}
