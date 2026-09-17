//! Retained, lazily constructed views over a pushed contiguous prefix.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::any::{Any, TypeId, type_name};

use aether_bloomery_kinds::{Entry, Seq};
use aether_bloomery_view::View;

use crate::error::{PrepareError, seq_mismatch};
use crate::params::Params;
use crate::trigger::Trigger;
use crate::views::{ErasedView, ViewCtor, ViewSet, box_view};

/// Portable owner of inferred view instances.
///
/// Callers push contiguous journal entries. Views are constructed on first
/// use, catch up from their trusted cursor, and stay poisoned after a failed
/// fold. Retained views must be `Send` so this owner can be held by an actor.
/// This type does not own a journal.
pub struct Owner {
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
        Self { prefix: Vec::new(), slots: BTreeMap::new() }
    }

    /// Last retained sequence, or [`Seq`] `(0)` when nothing has been pushed.
    #[must_use]
    pub fn cursor(&self) -> Seq {
        self.prefix.last().map_or(Seq(0), |entry| entry.seq)
    }

    /// Append the next contiguous entries. Does not fold; views catch up on
    /// prepare.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when `entries` is not the next dense sequence.
    pub fn push(&mut self, entries: &[Entry]) -> Result<(), PrepareError> {
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
        self.prefix.extend_from_slice(entries);
        Ok(())
    }

    /// One prepared event whose views are installed separately rather than folded
    /// from this prefix. Used by reactor peers that receive owned snapshots.
    #[must_use]
    pub fn from_prepared(entry: Entry) -> Self {
        Self { prefix: alloc::vec![entry], slots: BTreeMap::new() }
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

    /// Install an already-folded view at the current cursor.
    ///
    /// # Errors
    ///
    /// [`PrepareError::CursorContract`] when `view` is not at this prefix.
    pub fn install_published<V: View + Send + 'static>(&mut self, view: V) -> Result<(), PrepareError> {
        self.install_erased(TypeId::of::<V>(), type_name::<V>(), box_view(view))
    }

    pub(crate) fn install_erased(
        &mut self,
        id: TypeId,
        name: &'static str,
        boxed: Box<dyn ErasedView>,
    ) -> Result<(), PrepareError> {
        let target = self.cursor();
        let actual = boxed.cursor();
        if actual != target {
            return Err(PrepareError::CursorContract {
                view: name,
                last_trusted_cursor: actual,
                expected: target,
                actual,
            });
        }
        self.slots.insert(
            id,
            CachedView { poisoned: false, last_trusted_cursor: actual, type_name: name, inner: Some(boxed) },
        );
        Ok(())
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
        let last_trusted_cursor = self
            .slots
            .get(&ctor.id)
            .ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor: Seq(0) })?
            .last_trusted_cursor;
        if last_trusted_cursor >= target {
            return Ok(());
        }
        let entries = self.suffix(last_trusted_cursor, target)?.to_vec();
        let slot =
            self.slots.get_mut(&ctor.id).ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor })?;
        slot.poisoned = true;
        let advanced = slot
            .inner
            .as_mut()
            .ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor })?
            .advance(&entries);
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

    fn suffix(&self, after: Seq, through: Seq) -> Result<&[Entry], PrepareError> {
        let start = usize::try_from(after.0).map_err(|_| PrepareError::Overflow)?;
        let end = usize::try_from(through.0).map_err(|_| PrepareError::Overflow)?;
        if end > self.prefix.len() || start > end {
            return Err(seq_mismatch(Seq(after.0.saturating_add(1)), through));
        }
        Ok(&self.prefix[start..end])
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
