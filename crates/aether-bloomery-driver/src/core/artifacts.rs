//! The driver's artifact cache: found artifacts under a fixed byte budget.
//!
//! Artifacts are content-addressed and immutable, so a `Found` read stays
//! true for as long as the driver lives. The cache keeps each found
//! [`ClosureArtifact`] keyed by the digest it was read under, and evicts the
//! least recently used entry first once the budget is full. A member holds
//! its payload as a `Blob`, so a hit answers with a cheap clone, and every
//! reader of a cached member still verifies it through
//! [`ClosureArtifact::load`]. It never holds a missing artifact: one can be
//! stored after a miss.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use aether_bloomery_kinds::{ClosureArtifact, Digest};
use aether_data::KindId;

/// Byte budget of the driver's artifact cache: 64 MiB of cached payloads.
pub const ARTIFACT_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// One cached artifact and its last-use tick.
struct Cached {
    artifact: ClosureArtifact,
    tick: u64,
}

/// Found artifacts keyed by digest, evicted least recently used first.
pub struct ArtifactCache {
    budget_bytes: u64,
    held_bytes: u64,
    next_tick: u64,
    entries: HashMap<Digest, Cached>,
    /// Each entry's digest keyed by its last-use tick, oldest first.
    order: BTreeMap<u64, Digest>,
}

impl ArtifactCache {
    /// An empty cache that holds at most `budget_bytes` of payload.
    pub(crate) fn with_budget(budget_bytes: u64) -> Self {
        Self { budget_bytes, held_bytes: 0, next_tick: 0, entries: HashMap::new(), order: BTreeMap::new() }
    }

    /// The cached artifact, marked as just used.
    pub(crate) fn get(&mut self, digest: Digest) -> Option<&ClosureArtifact> {
        self.touch(digest)?;
        self.entries.get(&digest).map(|cached| &cached.artifact)
    }

    /// The cached artifact's kind, marked as just used.
    pub(crate) fn kind(&mut self, digest: Digest) -> Option<KindId> {
        self.touch(digest)
    }

    /// Cache one found artifact, evicting the least recently used entries
    /// until it fits.
    ///
    /// An artifact already cached is only marked as used, and one larger
    /// than the whole budget is not cached.
    pub(crate) fn insert(&mut self, digest: Digest, artifact: ClosureArtifact) {
        if self.touch(digest).is_some() {
            return;
        }
        let size = artifact.len();
        if size > self.budget_bytes {
            return;
        }
        while self.held_bytes + size > self.budget_bytes {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.held_bytes -= evicted.artifact.len();
            }
        }
        let tick = self.next_tick();
        self.order.insert(tick, digest);
        self.entries.insert(digest, Cached { artifact, tick });
        self.held_bytes += size;
    }

    /// Mark one cached entry as just used, returning its kind.
    fn touch(&mut self, digest: Digest) -> Option<KindId> {
        let tick = self.next_tick();
        let cached = self.entries.get_mut(&digest)?;
        self.order.remove(&cached.tick);
        cached.tick = tick;
        self.order.insert(tick, digest);
        Some(cached.artifact.kind())
    }

    fn next_tick(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick += 1;
        tick
    }
}

impl fmt::Debug for ArtifactCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactCache")
            .field("budget_bytes", &self.budget_bytes)
            .field("held_bytes", &self.held_bytes)
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ClosureArtifact, Digest};
    use aether_data::KindId;

    use super::ArtifactCache;

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn member(kind: u64, byte: u8, len: usize) -> ClosureArtifact {
        ClosureArtifact::new(KindId(kind), vec![byte; len])
    }

    #[test]
    fn eviction_is_least_recently_used_and_bytes_stay_within_budget() {
        // Catches byte-accounting drift that grows the cache past its budget,
        // or evicting the entry just used instead of the oldest.
        let mut cache = ArtifactCache::with_budget(100);
        let first = member(1, 1, 40);
        cache.insert(digest(1), first.clone());
        cache.insert(digest(2), member(2, 2, 40));
        assert_eq!(cache.kind(digest(1)), Some(KindId(1)));

        cache.insert(digest(3), member(3, 3, 40));
        assert!(cache.get(digest(2)).is_none(), "the untouched entry is evicted");
        let kept = cache.get(digest(1)).expect("the touched entry stays");
        assert_eq!(kept.load(first.claimed().unverified()), Ok(vec![1; 40]));
        assert_eq!(cache.kind(digest(3)), Some(KindId(3)));
        assert_eq!(cache.held_bytes, 80);
    }

    #[test]
    fn an_oversized_artifact_is_not_cached() {
        // Catches an oversized insert that evicts everything and then holds
        // more than the budget.
        let mut cache = ArtifactCache::with_budget(100);
        cache.insert(digest(1), member(1, 1, 40));
        cache.insert(digest(2), member(2, 2, 101));
        assert!(cache.get(digest(2)).is_none());
        assert_eq!(cache.kind(digest(1)), Some(KindId(1)), "the oversized insert evicts nothing");
        assert_eq!(cache.held_bytes, 40);
    }
}
