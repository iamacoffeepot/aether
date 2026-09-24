//! Generic entry that generated bundle code calls for one program.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, ReadArtifactResult, Ref, Refusal,
};
use aether_data::KindId;

use crate::declare::{AsyncProgram, SyncProgram};
use crate::env::{Async, EnvOwner, Pending, PendingArtifact, PendingCall};
use crate::kinds::Detail;

/// Run sync `P` against `invoke`. Never returns [`Invoked::Rejected`].
#[must_use]
pub fn invoke<P: SyncProgram>(invoke: Invoke) -> Invoked {
    let (seq, _, input, closure) = invoke.into_parts();
    match run_sync::<P>(input, closure) {
        Ok((result, staged)) => Invoked::Completed { seq, result, staged },
        Err(refusal) => Invoked::Refused { seq, refusal },
    }
}

fn run_sync<P: SyncProgram>(
    input: Digest,
    closure: Vec<ClosureArtifact>,
) -> Result<(Digest, Vec<EncodedArtifact>), Refusal> {
    let owner = EnvOwner::from_closure(closure);
    let mut env = owner.env();
    let input = env.injected(Ref::<P::Input>::from_digest(input))?;
    let result = P::run(input, &mut env)?;
    let result = env.stage_encoded(&result)?;
    refuse_orphans(&env.staged(), result.digest())?;
    Ok((result.digest(), env.into_staged()))
}

/// First poll of an invocation: done, or a live future the child must keep.
pub enum Started {
    /// The program completed, refused, or was rejected without awaiting.
    Finished(Invoked),
    /// The future yielded; keep [`AsyncSession`] until the next poll is [`PollResult::Finished`].
    Live { session: AsyncSession, waiting: Option<Pending> },
}

/// Result of polling a live session after a journal reply (or the first poll).
#[derive(Debug)]
pub enum PollResult {
    /// The program completed, refused, or was rejected.
    Finished(Invoked),
    /// Poll again after the child sends `ReadArtifact` for this digest.
    NeedArtifact(PendingArtifact),
    /// Poll again after the child [`PendingCall::dispatch`]es this call.
    NeedSend(PendingCall),
    /// A journal read is already in flight; wait for its reply.
    Waiting,
}

type RunFuture = Pin<Box<dyn Future<Output = Result<(Digest, Vec<EncodedArtifact>), Refusal>> + Send + 'static>>;

/// Live async `run` plus the environment it reads and stages through.
pub struct AsyncSession {
    owner: EnvOwner,
    seq: u64,
    future: RunFuture,
}

impl AsyncSession {
    /// Drive the future with a noop waker. The invocation child sends on
    /// [`PollResult::NeedArtifact`] and calls [`Self::fulfill`] on the reply.
    pub fn poll(&mut self) -> PollResult {
        match poll_once(self.future.as_mut()) {
            Poll::Ready(Ok((result, staged))) => {
                PollResult::Finished(Invoked::Completed { seq: self.seq, result, staged })
            }
            Poll::Ready(Err(refusal)) => PollResult::Finished(Invoked::Refused { seq: self.seq, refusal }),
            Poll::Pending => match self.owner.env::<Async>().take_pending() {
                Some(Pending::Artifact(pending)) => PollResult::NeedArtifact(pending),
                Some(Pending::Send(pending)) => PollResult::NeedSend(pending),
                None => PollResult::Waiting,
            },
        }
    }

    /// Apply a journal reply and make the next [`Self::poll`] see the digest.
    pub fn fulfill(&mut self, expected: PendingArtifact, result: ReadArtifactResult) {
        match &result {
            ReadArtifactResult::Found { digest, kind, .. }
                if *digest == expected.digest && *kind != expected.expected =>
            {
                self.owner.env::<Async>().fail(expected.digest, Refusal::InputDecode);
            }
            ReadArtifactResult::Found { digest, .. } if *digest != expected.digest => {
                self.owner.env::<Async>().fail(expected.digest, Refusal::InputDecode);
            }
            ReadArtifactResult::Missing { digest } if *digest != expected.digest => {
                self.owner.env::<Async>().fail(expected.digest, Refusal::InputDecode);
            }
            ReadArtifactResult::Err { digest, .. } if *digest != expected.digest => {
                self.owner.env::<Async>().fail(expected.digest, Refusal::InputDecode);
            }
            _ => self.owner.env::<Async>().fulfill(result),
        }
    }

    /// Apply a cap reply and make the next [`Self::poll`] see the decoded kind.
    pub fn fulfill_send(&mut self, expected: &PendingCall, kind: KindId, bytes: Vec<u8>) {
        if kind != expected.expected_reply {
            self.owner.env::<Async>().reject_call(Refusal::InputDecode);
            return;
        }
        self.owner.env::<Async>().fulfill_call(kind, bytes);
    }

    /// Fail the in-flight cap await (allowlist miss, or a dropped send).
    pub fn reject_send(&mut self, refusal: Refusal) {
        self.owner.env::<Async>().reject_call(refusal);
    }
}

/// Start async `P`. Input lookup is injected-only; cited blobs may fetch.
#[must_use]
pub fn start_async<P: AsyncProgram>(invoke: Invoke) -> Started {
    let (seq, _, input, closure) = invoke.into_parts();
    let owner = EnvOwner::from_closure(closure);
    let env = owner.env::<Async>();
    let input = match env.injected(Ref::<P::Input>::from_digest(input)) {
        Ok(input) => input,
        Err(refusal) => return Started::Finished(Invoked::Refused { seq, refusal }),
    };
    let mut session = AsyncSession {
        owner,
        seq,
        future: Box::pin(async move {
            let result = P::run(input, env).await?;
            let mut staged_env = env;
            let result = staged_env.stage_encoded(&result)?;
            refuse_orphans(&staged_env.staged(), result.digest())?;
            Ok((result.digest(), staged_env.into_staged()))
        }),
    };
    match session.poll() {
        PollResult::Finished(invoked) => Started::Finished(invoked),
        PollResult::NeedArtifact(pending) => Started::Live { session, waiting: Some(Pending::Artifact(pending)) },
        PollResult::NeedSend(pending) => Started::Live { session, waiting: Some(Pending::Send(pending)) },
        PollResult::Waiting => Started::Live { session, waiting: None },
    }
}

fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn refuse_orphans(staged: &[EncodedArtifact], root: Digest) -> Result<(), Refusal> {
    if let Some(digest) = unreachable_staged(staged, root) {
        return Err(Refusal::Refused {
            reason: Detail::new(format!("staged blob {digest} is not reachable from the result")),
        });
    }
    Ok(())
}

/// The first staged digest that `root` cannot reach through staged citations.
///
/// ADR-0224 §3: a completed invocation's staged set is valid whenever every
/// staged blob is reachable from the result, not only when the result is the
/// lone staged artifact. The walk is iterative and shared by every caller
/// that must check this rule — a program's own `refuse_orphans` and, per
/// ADR-0224 §7, the native driver re-checking a bundle's claim rather than
/// trusting it.
#[must_use]
pub fn unreachable_staged(staged: &[EncodedArtifact], root: Digest) -> Option<Digest> {
    let mut seen = Vec::new();
    let mut stack = alloc::vec![root];
    while let Some(digest) = stack.pop() {
        if seen.contains(&digest) {
            continue;
        }
        seen.push(digest);
        let Some(artifact) = staged.iter().find(|artifact| artifact.digest() == digest) else {
            continue;
        };
        for citation in artifact.citations() {
            let Ok(bytes) = <[u8; 32]>::try_from(citation.bytes()) else {
                continue;
            };
            let child = Digest::from_bytes(bytes);
            if staged.iter().any(|artifact| artifact.digest() == child) {
                stack.push(child);
            }
        }
    }
    staged.iter().map(EncodedArtifact::digest).find(|digest| !seen.contains(digest))
}
