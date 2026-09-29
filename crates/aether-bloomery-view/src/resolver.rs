//! Read-only typed artifact resolution for a live view fold.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll};

use aether_bloomery_kinds::{Digest, ReadArtifactResult, Ref};
use aether_data::{KindId, Storage};
use spin::Mutex;

/// One typed immutable artifact requested by a suspended fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingArtifact {
    /// Content digest the view requested.
    pub digest: Digest,
    /// Storage kind the typed reference requires.
    pub expected: KindId,
}

/// Terminal failure of an artifact-backed fold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The requested digest is absent.
    Missing { digest: Digest },
    /// The journal or transport refused the read.
    Transport { digest: Digest, message: String },
    /// The reply named a different digest than the request.
    DigestMismatch { expected: Digest, actual: Digest },
    /// The stored kind prefix differs from the typed reference.
    KindMismatch { digest: Digest, expected: KindId, actual: KindId },
    /// The complete kind-prefixed content did not hash to the reference.
    ContentMismatch { digest: Digest },
    /// The verified payload did not decode as the requested storage kind.
    Decode { digest: Digest, kind: KindId },
    /// A second read attempted to overtake the one active read.
    ConcurrentRead { active: Digest, requested: Digest },
    /// The operation ended while a read was still outstanding.
    Cancelled { digest: Digest },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { digest } => write!(f, "artifact {digest} is missing"),
            Self::Transport { digest, message } => write!(f, "artifact {digest} could not be read: {message}"),
            Self::DigestMismatch { expected, actual } => {
                write!(f, "artifact reply named {actual}, expected {expected}")
            }
            Self::KindMismatch { digest, expected, actual } => {
                write!(f, "artifact {digest} has kind {actual:?}, expected {expected:?}")
            }
            Self::ContentMismatch { digest } => write!(f, "artifact {digest} content does not match its reference"),
            Self::Decode { digest, kind } => write!(f, "artifact {digest} does not decode as {kind:?}"),
            Self::ConcurrentRead { active, requested } => {
                write!(f, "artifact read {requested} attempted to overtake active read {active}")
            }
            Self::Cancelled { digest } => write!(f, "artifact read {digest} was cancelled"),
        }
    }
}

impl Error for ResolveError {}

enum State {
    Idle,
    Requested(PendingArtifact),
    Waiting(PendingArtifact),
    Ready { pending: PendingArtifact, bytes: Vec<u8> },
    Terminal(ResolveError),
}

struct Inner {
    state: State,
}

/// Read-only artifact capability passed to an authored fold.
///
/// A resolver belongs to one fold operation. It permits one outstanding read
/// and retains no artifact after the typed value has been decoded.
#[derive(Clone)]
pub struct ArtifactResolver {
    inner: Arc<Mutex<Inner>>,
}

/// Owner-side half of one resolver operation.
///
/// Reactor ownership uses this half to discover a request and apply exactly
/// one correlated [`ReadArtifactResult`]. It is not an authoring capability.
#[doc(hidden)]
#[derive(Clone)]
pub struct ResolverDriver {
    inner: Arc<Mutex<Inner>>,
}

impl ArtifactResolver {
    /// Create the authoring and owner halves for one fold operation.
    #[doc(hidden)]
    #[must_use]
    pub fn operation() -> (Self, ResolverDriver) {
        let inner = Arc::new(Mutex::new(Inner { state: State::Idle }));
        (Self { inner: inner.clone() }, ResolverDriver { inner })
    }

    /// Resolve one immutable typed reference.
    pub fn read<K: Storage>(&mut self, reference: Ref<K>) -> Read<K> {
        Read {
            inner: self.inner.clone(),
            pending: PendingArtifact { digest: reference.digest(), expected: K::ID },
            phase: ReadPhase::Initial,
            _kind: PhantomData,
        }
    }

    /// Terminal validation failure observed by the owner before it trusts a
    /// view cursor. Authored folds cannot clear this state.
    #[doc(hidden)]
    #[must_use]
    pub fn terminal(&self) -> Option<ResolveError> {
        match &self.inner.lock().state {
            State::Terminal(error) => Some(error.clone()),
            State::Idle | State::Requested(_) | State::Waiting(_) | State::Ready { .. } => None,
        }
    }

    /// Finish a fold boundary, rejecting any read it retained outstanding.
    #[doc(hidden)]
    #[must_use]
    pub fn finish(&self) -> Option<ResolveError> {
        finish(&self.inner)
    }
}

enum ReadPhase {
    Initial,
    Waiting,
    Finished,
}

/// Future returned by [`ArtifactResolver::read`].
pub struct Read<K> {
    inner: Arc<Mutex<Inner>>,
    pending: PendingArtifact,
    phase: ReadPhase,
    _kind: PhantomData<fn() -> K>,
}

impl<K: Storage> Future for Read<K> {
    type Output = Result<K, ResolveError>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        let mut inner = this.inner.lock();
        match this.phase {
            ReadPhase::Initial => match &inner.state {
                State::Idle => {
                    inner.state = State::Requested(this.pending);
                    this.phase = ReadPhase::Waiting;
                    Poll::Pending
                }
                State::Terminal(error) => {
                    this.phase = ReadPhase::Finished;
                    Poll::Ready(Err(error.clone()))
                }
                State::Requested(active) | State::Waiting(active) | State::Ready { pending: active, .. } => {
                    let error = ResolveError::ConcurrentRead { active: active.digest, requested: this.pending.digest };
                    inner.state = State::Terminal(error.clone());
                    this.phase = ReadPhase::Finished;
                    Poll::Ready(Err(error))
                }
            },
            ReadPhase::Waiting => match &inner.state {
                State::Ready { pending, .. } if *pending == this.pending => {
                    let State::Ready { bytes, .. } = mem::replace(&mut inner.state, State::Idle) else {
                        unreachable!()
                    };
                    drop(inner);
                    if let Ok(data) = K::decode_storage(&bytes) {
                        this.phase = ReadPhase::Finished;
                        Poll::Ready(Ok(data.value))
                    } else {
                        let error = ResolveError::Decode { digest: this.pending.digest, kind: this.pending.expected };
                        this.inner.lock().state = State::Terminal(error.clone());
                        this.phase = ReadPhase::Finished;
                        Poll::Ready(Err(error))
                    }
                }
                State::Terminal(error) => {
                    this.phase = ReadPhase::Finished;
                    Poll::Ready(Err(error.clone()))
                }
                State::Requested(pending) | State::Waiting(pending) if *pending == this.pending => Poll::Pending,
                State::Idle | State::Requested(_) | State::Waiting(_) | State::Ready { .. } => {
                    let error = ResolveError::ConcurrentRead {
                        active: state_digest(&inner.state).unwrap_or(this.pending.digest),
                        requested: this.pending.digest,
                    };
                    inner.state = State::Terminal(error.clone());
                    this.phase = ReadPhase::Finished;
                    Poll::Ready(Err(error))
                }
            },
            ReadPhase::Finished => panic!("artifact read polled after completion"),
        }
    }
}

impl<K> Unpin for Read<K> {}

impl<K> Drop for Read<K> {
    fn drop(&mut self) {
        if !matches!(self.phase, ReadPhase::Waiting) {
            return;
        }
        let mut inner = self.inner.lock();
        if matches!(
            &inner.state,
            State::Requested(pending) | State::Waiting(pending) | State::Ready { pending, .. }
                if *pending == self.pending
        ) {
            inner.state = State::Terminal(ResolveError::Cancelled { digest: self.pending.digest });
        }
    }
}

impl ResolverDriver {
    /// Take a newly requested read for transport exactly once.
    #[must_use]
    pub fn take_pending(&self) -> Option<PendingArtifact> {
        let mut inner = self.inner.lock();
        let State::Requested(pending) = inner.state else {
            return None;
        };
        inner.state = State::Waiting(pending);
        Some(pending)
    }

    /// Apply the correlated transport result. Any validation failure is
    /// terminal for the whole fold, even when authored code ignores its read.
    pub fn fulfill(&self, result: ReadArtifactResult) {
        let mut inner = self.inner.lock();
        let pending = match inner.state {
            State::Waiting(pending) => pending,
            State::Terminal(_) | State::Idle | State::Requested(_) | State::Ready { .. } => return,
        };
        inner.state = match result {
            ReadArtifactResult::Found { artifact } => {
                let actual = artifact.claimed().unverified();
                if actual != pending.digest {
                    State::Terminal(ResolveError::DigestMismatch { expected: pending.digest, actual })
                } else if artifact.kind() != pending.expected {
                    State::Terminal(ResolveError::KindMismatch {
                        digest: pending.digest,
                        expected: pending.expected,
                        actual: artifact.kind(),
                    })
                } else {
                    artifact.load(pending.digest).map_or_else(
                        |_| State::Terminal(ResolveError::ContentMismatch { digest: pending.digest }),
                        |bytes| State::Ready { pending, bytes },
                    )
                }
            }
            ReadArtifactResult::Missing { digest } if digest == pending.digest => {
                State::Terminal(ResolveError::Missing { digest })
            }
            ReadArtifactResult::Err { digest, message } if digest == pending.digest => {
                State::Terminal(ResolveError::Transport { digest, message })
            }
            ReadArtifactResult::Missing { digest } | ReadArtifactResult::Err { digest, .. } => {
                State::Terminal(ResolveError::DigestMismatch { expected: pending.digest, actual: digest })
            }
        };
    }

    /// Current terminal failure, if validation or cancellation ended the fold.
    #[must_use]
    pub fn terminal(&self) -> Option<ResolveError> {
        match &self.inner.lock().state {
            State::Terminal(error) => Some(error.clone()),
            State::Idle | State::Requested(_) | State::Waiting(_) | State::Ready { .. } => None,
        }
    }

    /// Finish a fold boundary, rejecting any read it retained outstanding.
    #[must_use]
    pub fn finish(&self) -> Option<ResolveError> {
        finish(&self.inner)
    }

    /// Mark an outstanding read cancelled and release any transport payload.
    pub fn cancel(&self) {
        let mut inner = self.inner.lock();
        if let Some(digest) = state_digest(&inner.state) {
            inner.state = State::Terminal(ResolveError::Cancelled { digest });
        }
    }
}

fn state_digest(state: &State) -> Option<Digest> {
    match state {
        State::Requested(pending) | State::Waiting(pending) | State::Ready { pending, .. } => Some(pending.digest),
        State::Idle | State::Terminal(_) => None,
    }
}

fn finish(inner: &Arc<Mutex<Inner>>) -> Option<ResolveError> {
    let mut inner = inner.lock();
    match &inner.state {
        State::Idle => None,
        State::Terminal(error) => Some(error.clone()),
        State::Requested(pending) | State::Waiting(pending) | State::Ready { pending, .. } => {
            let error = ResolveError::Cancelled { digest: pending.digest };
            inner.state = State::Terminal(error.clone());
            Some(error)
        }
    }
}
