//! Fold over a contiguous journal prefix.

use core::error::Error;

use aether_bloomery_kinds::{Entry, Seq};

/// A fold over a contiguous journal prefix.
///
/// [`Self::empty`] starts at `Seq(0)`. A successful [`Self::advance`]
/// consumes every entry in the batch, including kinds the view ignores.
/// Views read only those entries and their own state. There is no
/// `Clone` / `Send` / `Sync` bound.
pub trait View: 'static {
    /// Failure to fold a batch.
    ///
    /// The owner of the fold reports this with the view type, last trusted
    /// cursor, and attempted range as needed.
    type Error: Error + 'static;

    /// Empty fold at `Seq(0)`.
    ///
    /// A nonzero cursor is a contract failure for the owner to refuse.
    fn empty() -> Self;

    /// Last consumed sequence, or `Seq(0)` when nothing has been applied.
    fn cursor(&self) -> Seq;

    /// Consume `entries` as the next contiguous prefix.
    ///
    /// # Errors
    ///
    /// [`Self::Error`] when the batch cannot be folded. The owner must treat a
    /// panic or a failed batch as leaving this view unusable.
    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error>;
}
