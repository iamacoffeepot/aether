//! Point-in-time views for single-writer, read-many state.

mod double_buffer;

use std::borrow::Borrow;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::sync::Arc;

use arc_swap::{ArcSwap, Guard};

pub use double_buffer::{DoubleBuffer, Update};

/// One immutable generation of a published value.
#[derive(Clone, Debug)]
pub struct Published<T> {
    table: T,
    generation: u64,
}

impl<T> Published<T> {
    fn new(table: T, generation: u64) -> Self {
        Self { table, generation }
    }

    /// Returns the value captured by this publication.
    #[must_use]
    pub fn table(&self) -> &T {
        &self.table
    }

    /// Returns the monotonically increasing publication generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// The publication generation can no longer advance without wrapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationExhausted;

impl fmt::Display for GenerationExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("view publication generation is exhausted")
    }
}

impl Error for GenerationExhausted {}

/// A cheap-to-clone read handle for a published value.
pub struct View<T> {
    shared: Arc<ArcSwap<Published<T>>>,
}

impl<T> Clone for View<T> {
    fn clone(&self) -> Self {
        Self { shared: Arc::clone(&self.shared) }
    }
}

impl<T> View<T> {
    /// Pins and returns the currently published generation.
    ///
    /// For views backed by [`DoubleBuffer`], keep the returned snapshot only
    /// for a short read. Extract or clone the small value needed and drop the
    /// snapshot before slow or blocking work, an await, actor-authored code, or
    /// two further publications. Holding it across the reuse boundary is
    /// diagnosed in debug builds and forces a coherent clone fallback in
    /// release builds.
    #[must_use]
    pub fn load(&self) -> Snapshot<T> {
        Snapshot { published: self.shared.load() }
    }
}

/// A pinned point-in-time publication.
///
/// A snapshot from a [`DoubleBuffer`]-backed view is a short-lived read guard:
/// extract or clone what is needed and release it before slow/blocking work,
/// awaiting, actor-authored code, or two more publication cycles.
pub struct Snapshot<T> {
    published: Guard<Arc<Published<T>>>,
}

impl<T> Snapshot<T> {
    /// Returns the value held by this snapshot.
    #[must_use]
    pub fn table(&self) -> &T {
        self.published.table()
    }

    /// Returns this snapshot's publication generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.published.generation()
    }
}

impl<K, V, S> Snapshot<HashMap<K, V, S>>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    /// Looks up one key in this point-in-time map.
    #[must_use]
    pub fn entry_for<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.table().get(key)
    }

    /// Enumerates every key-value pair in this point-in-time map.
    #[must_use]
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&K, &V)> {
        self.table().iter()
    }

    /// Enumerates every key in this point-in-time map.
    #[must_use]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> {
        self.table().keys()
    }

    /// Enumerates every value in this point-in-time map.
    #[must_use]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> {
        self.table().values()
    }

    /// Returns the number of entries in this point-in-time map.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table().len()
    }

    /// Returns whether this point-in-time map has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table().is_empty()
    }
}

/// The single-writer half of a published view.
pub struct ViewPublisher<T> {
    shared: Arc<ArcSwap<Published<T>>>,
    generation: u64,
}

impl<T> ViewPublisher<T> {
    /// Creates a publisher whose initial value is generation zero.
    #[must_use]
    pub fn new(initial: T) -> Self {
        Self { shared: Arc::new(ArcSwap::from_pointee(Published::new(initial, 0))), generation: 0 }
    }

    /// Creates another read handle for this publisher.
    #[must_use]
    pub fn view(&self) -> View<T> {
        View { shared: Arc::clone(&self.shared) }
    }

    /// Publishes a complete replacement value and returns its generation.
    pub fn publish(&mut self, table: T) -> Result<u64, GenerationExhausted> {
        let generation = self.next_generation()?;
        self.shared.store(Arc::new(Published::new(table, generation)));
        self.generation = generation;
        Ok(generation)
    }

    pub(super) fn next_generation(&self) -> Result<u64, GenerationExhausted> {
        self.generation.checked_add(1).ok_or(GenerationExhausted)
    }

    pub(super) fn swap_published(&mut self, published: Arc<Published<T>>, generation: u64) -> Arc<Published<T>> {
        debug_assert_eq!(published.generation, generation);
        self.generation = generation;
        self.shared.swap(published)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn whole_value_publication_advances_generations() {
        let mut publisher = ViewPublisher::new(String::from("zero"));
        let view = publisher.view();

        assert_eq!(view.load().generation(), 0);
        assert_eq!(publisher.publish(String::from("one")), Ok(1));
        assert_eq!(publisher.publish(String::from("two")), Ok(2));

        let snapshot = view.load();
        assert_eq!(snapshot.generation(), 2);
        assert_eq!(snapshot.table(), "two");
    }

    #[test]
    fn publication_refuses_to_wrap_generation() {
        let mut publisher = ViewPublisher::new(0);
        let view = publisher.view();
        publisher.generation = u64::MAX;

        assert_eq!(publisher.publish(1), Err(GenerationExhausted));
        assert_eq!(view.load().table(), &0);
    }

    #[test]
    fn superseded_snapshot_remains_valid() {
        let mut publisher = ViewPublisher::new(String::from("old"));
        let view = publisher.view();
        let old = view.load();

        publisher.publish(String::from("new")).expect("the second generation should be available");

        assert_eq!(old.table(), "old");
        assert_eq!(old.generation(), 0);
        assert_eq!(view.load().table(), "new");
    }

    #[test]
    fn superseded_value_drops_with_its_last_snapshot() {
        struct DropProbe(Arc<AtomicUsize>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let mut publisher = ViewPublisher::new(DropProbe(Arc::clone(&drops)));
        let view = publisher.view();
        let old = view.load();

        publisher.publish(DropProbe(Arc::clone(&drops))).expect("the second generation should be available");
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        drop(old);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}
