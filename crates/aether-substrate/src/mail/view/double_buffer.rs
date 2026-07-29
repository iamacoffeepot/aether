use std::hash::Hash;
use std::mem::take;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use super::{GenerationExhausted, Published, View, ViewPublisher};

/// One ordered change to a [`DoubleBuffer`].
pub enum Update<K, V> {
    /// Insert or replace a value.
    Insert(K, V),
    /// Remove a value when present.
    Remove(K),
    /// Mutate a present value exactly once.
    Mutate(K, Box<dyn FnOnce(&mut V) + Send + 'static>),
}

impl<K, V> Update<K, V> {
    /// Creates a one-shot mutation update.
    pub fn mutate(key: K, mutation: impl FnOnce(&mut V) + Send + 'static) -> Self {
        Self::Mutate(key, Box::new(mutation))
    }
}

enum Replay<K, V> {
    Insert(K, V),
    Remove(K),
}

impl<K, V> Replay<K, V>
where
    K: Eq + Hash,
{
    fn apply(self, table: &mut FxHashMap<K, V>) {
        match self {
            Self::Insert(key, value) => {
                table.insert(key, value);
            }
            Self::Remove(key) => {
                table.remove(&key);
            }
        }
    }
}

/// An ordered double-buffer publisher for map views.
///
/// Each batch updates the standby map with the previous replay lag and then
/// with the new updates. Publishing swaps the maps in constant time. Readers
/// must not pin a published snapshot across a complete two-publication cycle;
/// debug builds diagnose that contract at the reuse boundary. `Arc::make_mut`
/// remains a release-build safety valve that preserves coherent publication if
/// a reader violates the short-hold contract.
pub struct DoubleBuffer<K, V> {
    publisher: ViewPublisher<FxHashMap<K, V>>,
    standby: Arc<Published<FxHashMap<K, V>>>,
    replay_lag: Vec<Replay<K, V>>,
}

impl<K, V> DoubleBuffer<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    /// Creates two identical generation-zero buffers from an initial map.
    #[must_use]
    pub fn new(initial: FxHashMap<K, V>) -> Self {
        Self {
            publisher: ViewPublisher::new(initial.clone()),
            standby: Arc::new(Published::new(initial, 0)),
            replay_lag: Vec::new(),
        }
    }

    /// Creates another read handle for the published map.
    #[must_use]
    pub fn view(&self) -> View<FxHashMap<K, V>> {
        self.publisher.view()
    }

    /// Applies one ordered batch and publishes the resulting map.
    pub fn publish(&mut self, updates: impl IntoIterator<Item = Update<K, V>>) -> Result<u64, GenerationExhausted> {
        let generation = self.publisher.next_generation()?;
        debug_assert_eq!(
            Arc::strong_count(&self.standby),
            1,
            "DoubleBuffer publication-cycle violation: a reader pinned the standby snapshot across two publishes"
        );
        let replay_lag = take(&mut self.replay_lag);
        let published = Arc::make_mut(&mut self.standby);

        for replay in replay_lag {
            replay.apply(&mut published.table);
        }

        for update in updates {
            match update {
                Update::Insert(key, value) => {
                    published.table.insert(key.clone(), value.clone());
                    self.replay_lag.push(Replay::Insert(key, value));
                }
                Update::Remove(key) => {
                    published.table.remove(&key);
                    self.replay_lag.push(Replay::Remove(key));
                }
                Update::Mutate(key, mutation) => {
                    if let Some(value) = published.table.get_mut(&key) {
                        mutation(value);
                        self.replay_lag.push(Replay::Insert(key, value.clone()));
                    }
                }
            }
        }

        published.generation = generation;
        self.standby = self.publisher.swap_published(Arc::clone(&self.standby), generation);
        Ok(generation)
    }
}

impl<K, V> Default for DoubleBuffer<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    fn default() -> Self {
        Self::new(FxHashMap::default())
    }
}

#[cfg(test)]
mod tests {
    use std::iter::empty;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn entries(view: &View<FxHashMap<&'static str, i32>>) -> Vec<(&'static str, i32)> {
        let snapshot = view.load();
        let mut entries: Vec<_> = snapshot.entries().map(|(key, value)| (*key, *value)).collect();
        entries.sort_unstable();
        entries
    }

    #[test]
    fn insert_remove_order_is_preserved_within_and_across_batches() {
        let mut buffers = DoubleBuffer::default();
        let view = buffers.view();

        buffers
            .publish([Update::Insert("same", 1), Update::Remove("same")])
            .expect("the first generation should be available");
        assert!(view.load().entry_for("same").is_none());

        buffers.publish([Update::Insert("cross", 2)]).expect("the second generation should be available");
        buffers.publish([Update::Remove("cross")]).expect("the third generation should be available");
        assert!(view.load().is_empty());
    }

    #[test]
    fn register_drop_and_reregister_converges() {
        let mut buffers = DoubleBuffer::default();
        let view = buffers.view();

        buffers.publish([Update::Insert("actor", 1)]).expect("the first generation should be available");
        buffers.publish([Update::Remove("actor")]).expect("the second generation should be available");
        buffers.publish([Update::Insert("actor", 2)]).expect("the third generation should be available");

        let snapshot = view.load();
        assert_eq!(snapshot.entry_for("actor"), Some(&2));
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.keys().copied().collect::<Vec<_>>(), ["actor"]);
        assert_eq!(snapshot.values().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn generations_advance_and_exhaustion_preserves_the_published_map() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_update = Arc::clone(&calls);
        let mut buffers = DoubleBuffer::new(FxHashMap::from_iter([("actor", 1)]));
        let view = buffers.view();

        assert_eq!(buffers.publish([Update::Insert("other", 2)]), Ok(1));
        assert_eq!(view.load().generation(), 1);
        buffers.publisher.generation = u64::MAX;

        assert_eq!(
            buffers.publish([Update::mutate("actor", move |_| {
                calls_for_update.fetch_add(1, Ordering::Relaxed);
            })]),
            Err(GenerationExhausted)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(view.load().generation(), 1);
        assert_eq!(entries(&view), [("actor", 1), ("other", 2)]);
    }

    #[test]
    fn mutation_runs_once_and_replays_its_canonical_value() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_update = Arc::clone(&calls);
        let mut buffers = DoubleBuffer::new(FxHashMap::from_iter([("actor", 1)]));
        let view = buffers.view();

        buffers
            .publish([Update::mutate("actor", move |value| {
                calls_for_update.fetch_add(1, Ordering::Relaxed);
                *value += 4;
            })])
            .expect("the first generation should be available");
        buffers.publish(empty()).expect("the second generation should be available");

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(view.load().entry_for("actor"), Some(&5));
    }

    #[test]
    fn mutation_of_missing_key_has_no_replay() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_update = Arc::clone(&calls);
        let mut buffers = DoubleBuffer::default();
        let view = buffers.view();

        buffers
            .publish([Update::mutate("missing", move |_| {
                calls_for_update.fetch_add(1, Ordering::Relaxed);
            })])
            .expect("the first generation should be available");
        buffers.publish([Update::Insert("missing", 7)]).expect("the second generation should be available");
        buffers.publish(empty()).expect("the third generation should be available");

        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(view.load().entry_for("missing"), Some(&7));
    }

    #[test]
    fn short_held_snapshots_allow_alternating_buffer_reuse() {
        let mut buffers = DoubleBuffer::default();
        let view = buffers.view();

        buffers.publish([Update::Insert("a", 1)]).expect("the first generation should be available");
        assert_eq!(entries(&view), [("a", 1)]);

        buffers.publish([Update::Insert("b", 2)]).expect("the second generation should be available");
        assert_eq!(entries(&view), [("a", 1), ("b", 2)]);

        buffers
            .publish([Update::Remove("a"), Update::Insert("c", 3)])
            .expect("the third generation should be available");
        assert_eq!(entries(&view), [("b", 2), ("c", 3)]);

        buffers.publish(empty()).expect("the fourth generation should be available");
        assert_eq!(entries(&view), [("b", 2), ("c", 3)]);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "DoubleBuffer publication-cycle violation")]
    fn pinning_a_snapshot_across_its_reuse_cycle_panics() {
        let mut buffers = DoubleBuffer::default();
        let view = buffers.view();

        buffers.publish([Update::Insert("a", 1)]).expect("the first generation should be available");
        let _pinned = view.load();
        buffers.publish([Update::Insert("b", 2)]).expect("the second generation should be available");
        let _ = buffers.publish([Update::Insert("c", 3)]);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_clone_fallback_keeps_a_pinned_snapshot_and_new_view_coherent() {
        let mut buffers = DoubleBuffer::default();
        let view = buffers.view();

        buffers.publish([Update::Insert("a", 1)]).expect("the first generation should be available");
        let pinned = view.load();
        buffers.publish([Update::Insert("b", 2)]).expect("the second generation should be available");
        buffers.publish([Update::Insert("c", 3)]).expect("the third generation should be available");

        assert_eq!(pinned.generation(), 1);
        assert_eq!(pinned.entry_for("a"), Some(&1));
        assert_eq!(pinned.len(), 1);
        assert_eq!(entries(&view), [("a", 1), ("b", 2), ("c", 3)]);
    }
}
