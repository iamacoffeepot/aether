//! Owned journal plus one cached instance per native view type.

use std::any::{TypeId, type_name};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::ops::Range;

use aether_bloomery_journal::{Entry, Journal, JournalError, JournalIdentity, Seq};

use crate::error::ViewError;
use crate::selection::{ViewCtor, ViewSelection};
use crate::view::ErasedView;

/// Bounded page used when catching a view up to a requested prefix.
const READ_PAGE: usize = 256;

/// Owns one [`Journal`] and caches one native view instance per [`TypeId`].
pub struct ViewRegistry {
    journal: Journal,
    bound: JournalIdentity,
    slots: HashMap<TypeId, CachedView>,
    #[cfg(test)]
    remaining_ok_reads: Cell<Option<usize>>,
    #[cfg(test)]
    empty_next_read: Cell<bool>,
    #[cfg(test)]
    gap_next_read: Cell<bool>,
}

struct CachedView {
    poisoned: bool,
    last_trusted_cursor: Seq,
    type_name: &'static str,
    inner: Option<Box<dyn ErasedView>>,
}

/// Builder that has a view selection but no target prefix yet.
pub struct Unpositioned {
    _private: (),
}

/// Builder that has a target prefix. [`Views::with`] is available.
pub struct Positioned {
    at: Seq,
}

/// Selected views over a registry. [`Self::at`] is required before [`Self::with`].
pub struct Views<'a, S, P> {
    registry: &'a mut ViewRegistry,
    position: P,
    _selection: PhantomData<fn() -> S>,
}

impl ViewRegistry {
    /// Bind `journal`. Identity is captured now and checked before every view use.
    #[must_use]
    pub fn new(journal: Journal) -> Self {
        let bound = journal.identity();
        Self {
            journal,
            bound,
            slots: HashMap::new(),
            #[cfg(test)]
            remaining_ok_reads: Cell::new(None),
            #[cfg(test)]
            empty_next_read: Cell::new(false),
            #[cfg(test)]
            gap_next_read: Cell::new(false),
        }
    }

    /// Shared access to the bound journal.
    #[must_use]
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Mutable access to the bound journal.
    ///
    /// Existing drivers keep appending with `program::apply(&mut Journal, …)`.
    /// Replacing the journal with a different allocation fails closed on the
    /// next [`Views::with`]: caches are neither reused nor cleared.
    pub fn journal_mut(&mut self) -> &mut Journal {
        &mut self.journal
    }

    /// Drop cached views and return the bound journal.
    #[must_use]
    pub fn into_journal(self) -> Journal {
        self.journal
    }

    /// Select the views to inject. Does no replay.
    ///
    /// `with` is only available after [`Views::at`]:
    ///
    /// ```compile_fail
    /// use aether_bloomery_view::{Heads, ViewRegistry};
    /// fn demo(registry: &mut ViewRegistry) {
    ///     let _ = registry.views::<Heads>().with(|_heads| ());
    /// }
    /// ```
    #[must_use]
    pub fn views<S: ViewSelection>(&mut self) -> Views<'_, S, Unpositioned> {
        Views { registry: self, position: Unpositioned { _private: () }, _selection: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn fail_reads_after(&self, ok_reads: usize) {
        self.remaining_ok_reads.set(Some(ok_reads));
    }

    #[cfg(test)]
    pub(crate) fn empty_next_read(&self) {
        self.empty_next_read.set(true);
    }

    #[cfg(test)]
    pub(crate) fn gap_next_read(&self) {
        self.gap_next_read.set(true);
    }

    fn check_binding(&self) -> Result<(), ViewError> {
        if self.journal.identity() == self.bound {
            Ok(())
        } else {
            Err(ViewError::Binding)
        }
    }

    fn read_page(&self, since: Seq, limit: usize) -> Result<Vec<Entry>, JournalError> {
        #[cfg(test)]
        if self.empty_next_read.replace(false) {
            return Ok(Vec::new());
        }
        #[cfg(test)]
        if self.gap_next_read.replace(false) {
            let mut entries = self.journal.read(since, limit)?;
            if entries.len() >= 2 {
                let last = entries.pop().expect("gapped page has a last entry");
                entries.truncate(1);
                entries.push(last);
            }
            return Ok(entries);
        }
        #[cfg(test)]
        if let Some(left) = self.remaining_ok_reads.get() {
            if left == 0 {
                return Err(JournalError::IntegerRange);
            }
            self.remaining_ok_reads.set(Some(left - 1));
        }
        self.journal.read(since, limit)
    }

    fn unique_ctors<S: ViewSelection>() -> Vec<ViewCtor> {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        S::each_view(|ctor| {
            if seen.insert(ctor.id) {
                unique.push(ctor);
            }
        });
        unique
    }

    fn preflight(&self, ctors: &[ViewCtor], target: Seq) -> Result<(), ViewError> {
        let head = self.journal.head().map_err(ViewError::Head)?;
        if target > head {
            return Err(ViewError::BeyondHead { target, head });
        }
        for ctor in ctors {
            let Some(slot) = self.slots.get(&ctor.id) else {
                continue;
            };
            if slot.poisoned {
                return Err(ViewError::Poisoned {
                    view: slot.type_name,
                    last_trusted_cursor: slot.last_trusted_cursor,
                });
            }
            if slot.last_trusted_cursor > target {
                return Err(ViewError::Behind { view: slot.type_name, target, cursor: slot.last_trusted_cursor });
            }
        }
        Ok(())
    }

    fn ensure_constructed(&mut self, ctor: ViewCtor) -> Result<(), ViewError> {
        if self.slots.contains_key(&ctor.id) {
            return Ok(());
        }
        self.slots.insert(
            ctor.id,
            CachedView { poisoned: true, last_trusted_cursor: Seq(0), type_name: ctor.name, inner: None },
        );
        let boxed = (ctor.empty)();
        let cursor = boxed.cursor();
        let slot = self.slots.get_mut(&ctor.id).expect("poisoned slot inserted before empty");
        slot.inner = Some(boxed);
        if cursor != Seq(0) {
            return Err(ViewError::NonzeroEmpty { view: ctor.name, cursor });
        }
        slot.poisoned = false;
        Ok(())
    }

    fn catch_up(&mut self, ctor: ViewCtor, target: Seq) -> Result<(), ViewError> {
        loop {
            let last_trusted_cursor = self.trusted_cursor(ctor.id);
            if last_trusted_cursor >= target {
                return Ok(());
            }
            let limit = page_limit(last_trusted_cursor, target);
            let attempted = page_attempt(last_trusted_cursor, limit);
            let entries = self.read_page(last_trusted_cursor, limit).map_err(|source| ViewError::Read {
                view: ctor.name,
                last_trusted_cursor,
                attempted,
                source,
            })?;
            if entries.is_empty() {
                return Err(ViewError::Exhausted { view: ctor.name, last_trusted_cursor, target });
            }
            validate_page(ctor.name, last_trusted_cursor, target, &entries)?;
            let last = entries.last().expect("non-empty page").seq;
            if last < target && entries.len() < limit {
                return Err(ViewError::Exhausted { view: ctor.name, last_trusted_cursor, target });
            }
            self.advance_page(ctor, last_trusted_cursor, &entries)?;
        }
    }

    fn trusted_cursor(&self, id: TypeId) -> Seq {
        self.slots.get(&id).expect("view constructed before catch-up").last_trusted_cursor
    }

    fn advance_page(&mut self, ctor: ViewCtor, last_trusted_cursor: Seq, entries: &[Entry]) -> Result<(), ViewError> {
        let last = entries.last().expect("non-empty page").seq;
        let attempted = attempted_range(entries);
        let slot = self.slots.get_mut(&ctor.id).expect("view constructed before advance");
        slot.poisoned = true;
        match slot.inner.as_mut().expect("constructed view").advance(entries) {
            Ok(()) => {
                let actual = slot.inner.as_ref().expect("constructed view").cursor();
                if actual != last {
                    return Err(ViewError::CursorContract {
                        view: ctor.name,
                        last_trusted_cursor,
                        attempted,
                        expected: last,
                        actual,
                    });
                }
                slot.last_trusted_cursor = actual;
                slot.poisoned = false;
                Ok(())
            }
            Err(source) => Err(ViewError::Advance { view: ctor.name, last_trusted_cursor, attempted, source }),
        }
    }

    fn synchronize<S: ViewSelection>(&mut self, target: Seq) -> Result<(), ViewError> {
        self.check_binding()?;
        let ctors = Self::unique_ctors::<S>();
        self.preflight(&ctors, target)?;
        for ctor in &ctors {
            self.ensure_constructed(*ctor)?;
        }
        for ctor in ctors {
            self.catch_up(ctor, target)?;
        }
        Ok(())
    }
}

impl<'a, S: ViewSelection> Views<'a, S, Unpositioned> {
    /// Bind the exact journal prefix. Does no replay.
    #[must_use]
    pub fn at(self, position: Seq) -> Views<'a, S, Positioned> {
        Views { registry: self.registry, position: Positioned { at: position }, _selection: PhantomData }
    }
}

impl<S: ViewSelection> Views<'_, S, Positioned> {
    /// Create missing views, catch each up to the bound prefix, and inject them.
    ///
    /// The callback is synchronous and may return owned data. Borrowed view
    /// references cannot escape:
    ///
    /// ```compile_fail
    /// use aether_bloomery_journal::Seq;
    /// use aether_bloomery_view::{Heads, ViewRegistry};
    /// fn demo(registry: &mut ViewRegistry) -> &Heads {
    ///     registry.views::<Heads>().at(Seq(0)).with(|heads| heads).expect("seq 0")
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// [`ViewError`] when binding, preflight, construction, or catch-up fails.
    /// The callback is not invoked on failure.
    pub fn with<R>(self, f: impl for<'v> FnOnce(S::Refs<'v>) -> R) -> Result<R, ViewError> {
        let Views { registry, position, .. } = self;
        registry.synchronize::<S>(position.at)?;
        let refs =
            S::refs(|id| registry.slots.get(&id).and_then(|slot| slot.inner.as_ref()).map(|inner| inner.as_any()))
                .ok_or(ViewError::Poisoned { view: type_name::<S>(), last_trusted_cursor: Seq(0) })?;
        Ok(f(refs))
    }
}

fn page_limit(cursor: Seq, target: Seq) -> usize {
    let remaining = target.0.saturating_sub(cursor.0);
    usize::try_from(remaining).map_or(READ_PAGE, |remaining| remaining.min(READ_PAGE))
}

fn page_attempt(cursor: Seq, limit: usize) -> Range<Seq> {
    let start = Seq(cursor.0.saturating_add(1));
    let span = u64::try_from(limit).unwrap_or(u64::MAX);
    Seq(start.0)..Seq(start.0.saturating_add(span))
}

fn attempted_range(entries: &[Entry]) -> Range<Seq> {
    let first = entries.first().expect("non-empty page").seq;
    let last = entries.last().expect("non-empty page").seq;
    first..Seq(last.0.saturating_add(1))
}

fn validate_page(view: &'static str, cursor: Seq, target: Seq, entries: &[Entry]) -> Result<(), ViewError> {
    let Some(mut expected) = cursor.0.checked_add(1) else {
        return Err(ViewError::Exhausted { view, last_trusted_cursor: cursor, target });
    };
    for entry in entries {
        if entry.seq > target {
            return Err(ViewError::InvalidRange {
                view,
                last_trusted_cursor: cursor,
                attempted: attempted_range(entries),
                expected: target,
                actual: entry.seq,
            });
        }
        if entry.seq.0 != expected {
            return Err(ViewError::InvalidRange {
                view,
                last_trusted_cursor: cursor,
                attempted: attempted_range(entries),
                expected: Seq(expected),
                actual: entry.seq,
            });
        }
        expected = match expected.checked_add(1) {
            Some(next) => next,
            None => break,
        };
    }
    Ok(())
}

#[cfg(test)]
mod read_failures {
    use std::convert::Infallible;
    use std::error::Error;

    use aether_bloomery_journal::{Batch, Clock, Draft, Journal, Seq};

    use super::{READ_PAGE, ViewRegistry};
    use crate::error::ViewError;
    use crate::view::View;

    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now_millis(&self) -> u64 {
            self.0
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.view.read_note")]
    struct Note {
        n: u64,
    }

    struct Count {
        cursor: Seq,
        seen: u64,
    }

    impl View for Count {
        type Error = Infallible;

        fn empty() -> Self {
            Self { cursor: Seq(0), seen: 0 }
        }

        fn cursor(&self) -> Seq {
            self.cursor
        }

        fn advance(&mut self, entries: &[aether_bloomery_journal::Entry]) -> Result<(), Self::Error> {
            self.seen += u64::try_from(entries.len()).expect("page fits u64");
            if let Some(last) = entries.last() {
                self.cursor = last.seq;
            }
            Ok(())
        }
    }

    fn append_notes(journal: &mut Journal, expect: Seq, count: u64) -> Result<Seq, Box<dyn Error>> {
        let mut batch = Batch::new();
        for n in 0..count {
            batch.push_draft(Draft::of(&Note { n }, None)?);
        }
        let range = journal.append(expect, &batch)?;
        Ok(Seq(range.end.0.saturating_sub(1)))
    }

    #[test]
    fn a_read_failure_keeps_a_successful_prefix_and_does_not_poison_or_call_back() -> Result<(), Box<dyn Error>> {
        // Bug: a backend read error poisons the view, rebuilds it, or still runs the callback.
        let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
        let head = append_notes(&mut journal, Seq(0), u64::try_from(READ_PAGE + 3)?)?;
        let mut registry = ViewRegistry::new(journal);
        registry.fail_reads_after(1);
        let mut called = false;
        let error = registry
            .views::<Count>()
            .at(head)
            .with(|count| {
                called = true;
                count.seen
            })
            .expect_err("injected read failure");
        assert!(!called, "callback must not run on read failure");
        assert!(matches!(error, ViewError::Read { .. }), "{error:?}");

        let seen = registry.views::<Count>().at(Seq(u64::try_from(READ_PAGE)?)).with(|count| count.seen)?;
        assert_eq!(seen, u64::try_from(READ_PAGE)?);
        Ok(())
    }

    #[test]
    fn a_later_view_read_failure_retains_the_earlier_view_prefix() -> Result<(), Box<dyn Error>> {
        // Bug: a sibling read failure rolls back or poisons a view that already caught up.
        struct Other {
            cursor: Seq,
            seen: u64,
        }

        impl View for Other {
            type Error = Infallible;

            fn empty() -> Self {
                Self { cursor: Seq(0), seen: 0 }
            }

            fn cursor(&self) -> Seq {
                self.cursor
            }

            fn advance(&mut self, entries: &[aether_bloomery_journal::Entry]) -> Result<(), Self::Error> {
                self.seen += u64::try_from(entries.len()).expect("page fits u64");
                if let Some(last) = entries.last() {
                    self.cursor = last.seq;
                }
                Ok(())
            }
        }

        let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
        let head = append_notes(&mut journal, Seq(0), 4)?;
        let mut registry = ViewRegistry::new(journal);
        registry.fail_reads_after(1);
        let mut called = false;
        let error = registry
            .views::<(Count, Other)>()
            .at(head)
            .with(|_| {
                called = true;
            })
            .expect_err("second view read fails");
        assert!(!called);
        assert!(matches!(error, ViewError::Read { .. }), "{error:?}");

        let seen = registry.views::<Count>().at(head).with(|count| count.seen)?;
        assert_eq!(seen, 4);
        Ok(())
    }

    #[test]
    fn an_exhausted_read_before_target_is_an_error_not_synchronization() -> Result<(), Box<dyn Error>> {
        // Bug: a short empty read is treated as "already caught up" and the callback still runs.
        let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
        let head = append_notes(&mut journal, Seq(0), 3)?;
        let mut registry = ViewRegistry::new(journal);
        registry.empty_next_read();
        let mut called = false;
        let error = registry
            .views::<Count>()
            .at(head)
            .with(|_| {
                called = true;
            })
            .expect_err("empty page before target");
        assert!(!called);
        assert!(matches!(error, ViewError::Exhausted { target, .. } if target == head), "{error:?}");
        Ok(())
    }

    #[test]
    fn a_gapped_page_is_invalid_and_does_not_poison() -> Result<(), Box<dyn Error>> {
        // Bug: a non-dense page is fed to advance, or the view is poisoned as if the fold failed.
        let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
        let head = append_notes(&mut journal, Seq(0), 3)?;
        let mut registry = ViewRegistry::new(journal);
        registry.gap_next_read();
        let mut called = false;
        let error = registry
            .views::<Count>()
            .at(head)
            .with(|_| {
                called = true;
            })
            .expect_err("gapped page");
        assert!(!called);
        assert!(
            matches!(error, ViewError::InvalidRange { expected, actual, .. } if expected == Seq(2) && actual == head),
            "{error:?}"
        );
        let seen = registry.views::<Count>().at(head).with(|count| count.seen)?;
        assert_eq!(seen, 3);
        Ok(())
    }
}
