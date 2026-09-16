//! Fold over a contiguous journal prefix.

#[cfg(feature = "native")]
use core::any::Any;
use core::error::Error;

use aether_bloomery_journal::{Entry, Seq};

/// A fold over a contiguous journal prefix.
///
/// [`Self::empty`] starts at `Seq(0)`. A successful [`Self::advance`]
/// consumes every entry in the batch, including kinds the view ignores.
/// Views read only those entries and their own state. There is no
/// `Clone` / `Send` / `Sync` bound.
pub trait View: 'static {
    /// Failure to fold a batch. Reported by the registry with the view type
    /// name, last trusted cursor, and attempted range.
    type Error: Error + 'static;

    /// Empty fold at `Seq(0)`.
    ///
    /// A nonzero cursor poisons the cached slot.
    fn empty() -> Self;

    /// Last consumed sequence, or `Seq(0)` when nothing has been applied.
    fn cursor(&self) -> Seq;

    /// Consume `entries` as the next contiguous prefix.
    ///
    /// # Errors
    ///
    /// [`Self::Error`] when the batch cannot be folded. The registry marks the
    /// cached slot poisoned before this call, so a panic or a failed batch
    /// leaves the view unusable.
    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error>;
}

#[cfg(feature = "native")]
pub trait ErasedView {
    fn cursor(&self) -> Seq;
    fn as_any(&self) -> &dyn Any;
    fn advance(&mut self, entries: &[Entry]) -> Result<(), Box<dyn Error + 'static>>;
}

#[cfg(feature = "native")]
pub struct Slot<V: View> {
    pub view: V,
}

#[cfg(feature = "native")]
impl<V: View> ErasedView for Slot<V> {
    fn cursor(&self) -> Seq {
        self.view.cursor()
    }

    fn as_any(&self) -> &dyn Any {
        &self.view
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Box<dyn Error + 'static>> {
        self.view.advance(entries).map_err(|error| Box::new(error) as _)
    }
}
