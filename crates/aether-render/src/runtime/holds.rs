//! What a resource registry keeps for the draw sets that name its
//! entries (ADR-0246 decision 2): a count of the sets naming each id,
//! and the entries that were destroyed while a set still named them.
//!
//! The registry stays the only owner of an entry. A destroyed entry
//! leaves the registry's live map, so its id answers no lookup by id,
//! and when a set holds it the entry moves here whole instead of being
//! dropped. It stays until the last set lets go. The invariant every
//! caller relies on: a held id is in the registry's live map or in
//! `retired`, never in both and never in neither.

use std::collections::HashMap;

pub(super) struct Holds<T> {
    counts: HashMap<u32, u32>,
    retired: HashMap<u32, T>,
}

impl<T> Default for Holds<T> {
    fn default() -> Self {
        Self { counts: HashMap::new(), retired: HashMap::new() }
    }
}

impl<T> Holds<T> {
    /// One more set names `id`.
    pub(super) fn hold(&mut self, id: u32) {
        *self.counts.entry(id).or_insert(0) += 1;
    }

    /// One set no longer names `id`. When that was the last, a retired
    /// entry is dropped; the return says whether one was.
    ///
    /// # Panics
    /// Panics on an id no set holds, fail-fast per ADR-0063: a release
    /// without its hold means some set's accounting is already wrong.
    pub(super) fn release(&mut self, id: u32) -> bool {
        let count = self.counts.get_mut(&id).expect("a draw set releases only an id it holds");
        *count -= 1;
        if *count > 0 {
            return false;
        }

        self.counts.remove(&id);
        self.retired.remove(&id).is_some()
    }

    /// Whether any set names `id`.
    pub(super) fn is_held(&self, id: u32) -> bool {
        self.counts.contains_key(&id)
    }

    /// Keep a destroyed entry for the sets still naming `id`.
    pub(super) fn retire(&mut self, id: u32, entry: T) {
        self.retired.insert(id, entry);
    }

    /// The entry destroyed under `id` while a set named it, if it is
    /// still held.
    pub(super) fn retired(&self, id: u32) -> Option<&T> {
        self.retired.get(&id)
    }

    /// [`Self::retired`], to change the entry.
    pub(super) fn retired_mut(&mut self, id: u32) -> Option<&mut T> {
        self.retired.get_mut(&id)
    }

    /// Every retired entry, for work that must reach each entry a
    /// registry still owns, live or not.
    pub(super) fn retired_entries_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.retired.values_mut()
    }
}
