//! Retained, lazily constructed views over a pushed contiguous prefix.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::any::{Any, TypeId, type_name};

use aether_bloomery_kinds::{Entry, Seq};

use crate::error::{PrepareError, seq_mismatch};
use crate::params::Params;
use crate::trigger::Trigger;
use crate::views::{ErasedView, ViewCtor, ViewSet};

/// Portable owner of inferred view instances.
///
/// Callers push contiguous journal entries. Views are constructed on first
/// use, catch up from their trusted cursor, and stay poisoned after a failed
/// fold. Retained views must be `Send` so this owner can be held by an actor.
/// This type does not own a journal. The crate's [`crate::Root`] releases
/// entries at or below every constructed view's cursor and keeps the last one
/// as the trigger.
pub struct Owner {
    base: Seq,
    prefix: Vec<Entry>,
    slots: BTreeMap<TypeId, CachedView>,
}

struct CachedView {
    poisoned: bool,
    last_trusted_cursor: Seq,
    type_name: &'static str,
    inner: Option<Box<dyn ErasedView>>,
}

impl Owner {
    /// Empty prefix at [`Seq`] `(0)`, no views constructed.
    #[must_use]
    pub const fn new() -> Self {
        Self { base: Seq(0), prefix: Vec::new(), slots: BTreeMap::new() }
    }

    /// Last retained sequence, or [`Seq`] `(0)` when nothing has been pushed.
    #[must_use]
    pub fn cursor(&self) -> Seq {
        self.prefix.last().map_or(self.base, |entry| entry.seq)
    }

    /// Whether any constructed view is unusable after a failed fold.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.slots.values().any(|slot| slot.poisoned)
    }

    /// Append the next contiguous entries. Does not fold; views catch up on
    /// prepare.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when `entries` is not the next dense sequence.
    pub fn push(&mut self, entries: &[Entry]) -> Result<(), PrepareError> {
        self.check_next(entries)?;
        self.prefix.extend_from_slice(entries);
        Ok(())
    }

    /// [`Self::push`] that moves `entries` in rather than cloning them.
    pub(crate) fn push_owned(&mut self, entries: Vec<Entry>) -> Result<(), PrepareError> {
        self.check_next(&entries)?;
        self.prefix.extend(entries);
        Ok(())
    }

    /// Drop every retained entry each constructed view has folded, keeping the
    /// last one as the trigger. With no constructed view, keep only the last.
    pub(crate) fn release_folded(&mut self) {
        let cursor = self.cursor();
        let folded = self
            .slots
            .values()
            .filter(|slot| !slot.poisoned)
            .map(|slot| slot.last_trusted_cursor)
            .min()
            .map_or(cursor, |slowest| slowest.min(cursor));
        let released = usize::try_from(folded.0.saturating_sub(self.base.0))
            .unwrap_or(usize::MAX)
            .min(self.prefix.len().saturating_sub(1));
        if let Some(last) = self.prefix.drain(..released).next_back() {
            self.base = last.seq;
        }
    }

    fn check_next(&self, entries: &[Entry]) -> Result<(), PrepareError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut expected = match self.cursor().0.checked_add(1) {
            Some(next) => Seq(next),
            None => return Err(PrepareError::Overflow),
        };
        for entry in entries {
            if entry.seq != expected {
                return Err(seq_mismatch(expected, entry.seq));
            }
            expected = Seq(expected.0.checked_add(1).ok_or(PrepareError::Overflow)?);
        }
        Ok(())
    }

    /// Fold `S` to the current cursor, constructing missing views from empty.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when a view cannot be constructed or advanced.
    pub fn warm<S: ViewSet>(&mut self) -> Result<(), PrepareError> {
        self.catch_up::<S>()
    }

    /// Borrow a constructed, unpoisoned view.
    #[must_use]
    pub fn get<V: 'static>(&self) -> Option<&V> {
        self.slot_ref(TypeId::of::<V>())?.downcast_ref()
    }

    /// Decode the last retained entry as `T` and resolve an inferred parameter
    /// list at the current prefix. Already-constructed views advance only the
    /// new suffix. [`None`] means a named guard declined.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when the trigger, catch-up, or a poisoned view fails.
    pub fn prepare<T: Trigger, L: Params<T>>(&mut self) -> Result<Option<(T, L::Value)>, PrepareError> {
        let trigger = T::from_entry(self.prefix.last().ok_or(PrepareError::Empty)?).map_err(PrepareError::Trigger)?;
        self.catch_up::<L::Views>()?;
        let refs = L::Views::refs(|id| self.slot_ref(id))
            .ok_or(PrepareError::Poisoned { view: type_name::<L::Views>(), last_trusted_cursor: self.cursor() })?;
        Ok(L::resolve(&trigger, refs).map(|value| (trigger, value)))
    }

    fn catch_up<S: ViewSet>(&mut self) -> Result<(), PrepareError> {
        let target = self.cursor();
        for ctor in unique_ctors::<S>() {
            self.ensure_constructed(ctor)?;
            self.advance_to(ctor, target)?;
        }
        Ok(())
    }

    fn ensure_constructed(&mut self, ctor: ViewCtor) -> Result<(), PrepareError> {
        if let Some(slot) = self.slots.get(&ctor.id) {
            if slot.poisoned {
                return Err(PrepareError::Poisoned {
                    view: slot.type_name,
                    last_trusted_cursor: slot.last_trusted_cursor,
                });
            }
            return Ok(());
        }
        self.slots.insert(
            ctor.id,
            CachedView { poisoned: true, last_trusted_cursor: Seq(0), type_name: ctor.name, inner: None },
        );
        let boxed = (ctor.empty)();
        let cursor = boxed.cursor();
        let slot = self
            .slots
            .get_mut(&ctor.id)
            .ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor: Seq(0) })?;
        slot.inner = Some(boxed);
        if cursor != Seq(0) {
            return Err(PrepareError::NonzeroEmpty { view: ctor.name, cursor });
        }
        slot.poisoned = false;
        Ok(())
    }

    fn advance_to(&mut self, ctor: ViewCtor, target: Seq) -> Result<(), PrepareError> {
        let slot = self
            .slots
            .get_mut(&ctor.id)
            .ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor: Seq(0) })?;
        let last_trusted_cursor = slot.last_trusted_cursor;
        if last_trusted_cursor >= target {
            return Ok(());
        }
        let entries = suffix(&self.prefix, self.base, last_trusted_cursor, target)?;
        slot.poisoned = true;
        let advanced = slot
            .inner
            .as_mut()
            .ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor })?
            .advance(entries);
        match advanced {
            Ok(()) => {
                let actual = slot
                    .inner
                    .as_ref()
                    .ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor })?
                    .cursor();
                if actual != target {
                    return Err(PrepareError::CursorContract {
                        view: ctor.name,
                        last_trusted_cursor,
                        expected: target,
                        actual,
                    });
                }
                slot.last_trusted_cursor = actual;
                slot.poisoned = false;
                Ok(())
            }
            Err(source) => Err(PrepareError::Advance { view: ctor.name, last_trusted_cursor, source }),
        }
    }

    #[cfg(test)]
    pub(crate) fn retained(&self) -> usize {
        self.prefix.len()
    }

    pub(crate) fn slot_ref(&self, id: TypeId) -> Option<&dyn Any> {
        let slot = self.slots.get(&id)?;
        if slot.poisoned {
            return None;
        }
        slot.inner.as_ref().map(|inner| inner.as_any())
    }
}

impl Default for Owner {
    fn default() -> Self {
        Self::new()
    }
}

/// Retained entries after `after` through `through`, where `prefix[0]` is the
/// entry after `base`. An `after` below `base` was released and fails like an
/// out-of-range suffix.
fn suffix(prefix: &[Entry], base: Seq, after: Seq, through: Seq) -> Result<&[Entry], PrepareError> {
    let mismatch = || seq_mismatch(Seq(after.0.saturating_add(1)), through);
    let start =
        usize::try_from(after.0.checked_sub(base.0).ok_or_else(mismatch)?).map_err(|_| PrepareError::Overflow)?;
    let end =
        usize::try_from(through.0.checked_sub(base.0).ok_or_else(mismatch)?).map_err(|_| PrepareError::Overflow)?;
    if end > prefix.len() || start > end {
        return Err(mismatch());
    }
    Ok(&prefix[start..end])
}

fn unique_ctors<S: ViewSet>() -> Vec<ViewCtor> {
    let mut unique = Vec::new();
    let mut seen = BTreeSet::new();
    S::each_view(|ctor| {
        if seen.insert(ctor.id) {
            unique.push(ctor);
        }
    });
    unique
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::error::Error;
    use core::fmt;
    use core::ops::RangeInclusive;

    use aether_bloomery_kinds::{Entry, Seq};
    use aether_bloomery_view::View;
    use aether_data::KindId;

    use super::Owner;

    /// Counts folded entries and refuses a batch that skips or repeats one.
    struct Counter<const TAG: u8> {
        cursor: Seq,
        folded: u64,
    }

    #[derive(Debug)]
    struct Gap;

    impl fmt::Display for Gap {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("batch does not start after the cursor")
        }
    }

    impl Error for Gap {}

    impl<const TAG: u8> View for Counter<TAG> {
        type Error = Gap;

        fn empty() -> Self {
            Self { cursor: Seq(0), folded: 0 }
        }

        fn cursor(&self) -> Seq {
            self.cursor
        }

        fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
            if entries.first().is_some_and(|first| first.seq.0 != self.cursor.0 + 1) {
                return Err(Gap);
            }
            self.folded += entries.len() as u64;
            if let Some(last) = entries.last() {
                self.cursor = last.seq;
            }
            Ok(())
        }
    }

    type A = Counter<0>;
    type B = Counter<1>;

    fn entries(seqs: RangeInclusive<u64>) -> Vec<Entry> {
        seqs.map(|seq| Entry { seq: Seq(seq), kind: KindId(1), cause: None, recorded_at_millis: 0, bytes: Vec::new() })
            .collect()
    }

    #[test]
    fn release_keeps_the_trigger_and_views_keep_folding() {
        // Catches a dropped trigger, an off-by-one in `base`, and a cursor that falls back to 0.
        let mut owner = Owner::new();
        owner.push(&entries(1..=300)).expect("dense");
        owner.warm::<A>().expect("fold");
        owner.release_folded();
        assert_eq!(owner.retained(), 1);
        assert_eq!(owner.cursor(), Seq(300));

        owner.push(&entries(301..=302)).expect("dense");
        owner.warm::<A>().expect("fold");
        let view = owner.get::<A>().expect("constructed");
        assert_eq!((view.cursor, view.folded), (Seq(302), 302));
    }

    #[test]
    fn release_holds_entries_a_lagging_view_still_needs() {
        // Catches releasing past the slowest view's cursor.
        let mut owner = Owner::new();
        owner.push(&entries(1..=5)).expect("dense");
        owner.warm::<A>().expect("fold");
        owner.push(&entries(6..=10)).expect("dense");
        owner.warm::<B>().expect("fold");
        owner.release_folded();
        assert_eq!(owner.retained(), 5);

        owner.warm::<A>().expect("fold");
        let view = owner.get::<A>().expect("constructed");
        assert_eq!((view.cursor, view.folded), (Seq(10), 10));
    }
}
