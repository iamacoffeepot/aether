//! Fold over a contiguous journal prefix.

use core::error::Error;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use aether_bloomery_kinds::{Entry, Seq};

use crate::ArtifactResolver;

/// A fold over a contiguous journal prefix.
///
/// [`Self::empty`] starts at `Seq(0)`. A successful [`Self::advance`]
/// consumes every entry in the batch, including kinds the view ignores.
/// Views read only those entries, immutable artifacts named by them, and
/// their own state. There is no `Clone` / `Send` / `Sync` bound on manually
/// authored views.
pub trait View: 'static {
    /// Failure to fold a batch.
    ///
    /// The owner of the fold reports this with the view type, last trusted
    /// cursor, and attempted range as needed.
    type Error: Error + 'static;

    /// Awaitable calculation returned by [`Self::advance`].
    type Advance<'a>: Future<Output = Result<(), Self::Error>> + 'a
    where
        Self: 'a;

    /// Empty fold at `Seq(0)`.
    ///
    /// A nonzero cursor is a contract failure for the owner to refuse.
    fn empty() -> Self;

    /// Last consumed sequence, or `Seq(0)` when nothing has been applied.
    fn cursor(&self) -> Seq;

    /// Consume `entries` as the next contiguous prefix, resolving immutable
    /// artifacts through `artifacts` when the fold needs them.
    ///
    /// # Errors
    ///
    /// [`Self::Error`] when the batch cannot be folded. The owner must treat a
    /// panic or a failed batch as leaving this view unusable.
    fn advance<'a>(&'a mut self, entries: &'a [Entry], artifacts: &'a mut ArtifactResolver) -> Self::Advance<'a>;

    /// Drive a fold that is known not to await an artifact.
    ///
    /// Direct synchronous consumers use this compatibility helper. A view
    /// that requests an artifact must run under the reactor owner's retained
    /// continuation instead.
    ///
    /// # Panics
    ///
    /// Panics when the fold suspends.
    fn advance_ready(&mut self, entries: &[Entry]) -> Result<(), Self::Error>
    where
        Self: Sized,
    {
        let (mut artifacts, _driver) = ArtifactResolver::operation();
        let mut future = pin!(self.advance(entries, &mut artifacts));
        match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("artifact-backed view advance requires a retained reactor owner"),
        }
    }
}
