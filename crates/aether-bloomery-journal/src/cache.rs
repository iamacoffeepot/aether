//! The journal actor's read cache: verified members it checked in, kept
//! resident across reads under a byte budget.
//!
//! This is a cache, so eviction is correct: no reply depends on an entry,
//! and a member evicted under the budget is read from disk again. It holds
//! the [`aether_data::Blob`]s the journal itself checked in, keyed by the
//! digest each was stored under, so the engine blob store gains no lookup by
//! hash (ADR-0238 decision 4).
//!
//! Entries are grouped by the check-in that allocated them: one group per
//! closure read's miss slab, one single-member group per artifact read. A
//! group is charged its allocation's bytes, the sum of its members' payload
//! lengths, because one cached member keeps its whole slab resident
//! (ADR-0238 decision 8). A hit touches its member's whole group, and
//! eviction removes whole groups, least recently used first, so the members
//! of one slab still live and die together.
//!
//! A hit is not verified again. Its claim was computed from the bytes and
//! compared with the stored digest when it entered ([`Verified::check`]), a
//! blob's bytes are immutable from check-in (ADR-0238 decision 1), and the
//! journal never deletes or rewrites a stored artifact, so the claim stays
//! true for as long as the entry lives. The receiver still checks every
//! member it loads (ADR-0238 decision 11).
//!
//! The workers share one [`ReadCache`] and lock it only for map work: disk
//! reads, hashing, check-in, and dropping released members all run outside
//! the lock.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use aether_bloomery_kinds::ClosureArtifact;

use crate::Digest;
use crate::closure::Verified;

/// The journal actor's read-cache budget in bytes, charged per check-in
/// allocation rather than per member.
///
/// `0` caches nothing. The default, [`Self::DEFAULT_BYTES`] (2 GiB), fits a
/// closure's first read whole: one slab covering an environment (about
/// 950 MB) plus its source tree, with headroom for later source-tree groups
/// over the same hot environment. A group larger than the budget is not
/// cached at all.
#[derive(Clone, Copy, Debug)]
pub struct ReadCacheBudget(u64);

impl ReadCacheBudget {
    /// The default budget: 2 GiB.
    pub const DEFAULT_BYTES: u64 = 2_147_483_648;

    /// A budget of `bytes`. Every value is a budget, and `0` caches nothing.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// The budget in bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl Default for ReadCacheBudget {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BYTES)
    }
}

/// A cheap-`Clone` handle to the members the journal checked in, shared by
/// every read worker.
#[derive(Clone)]
pub struct ReadCache {
    state: Arc<Mutex<State>>,
}

impl ReadCache {
    pub(crate) fn with_budget(budget: ReadCacheBudget) -> Self {
        Self { state: Arc::new(Mutex::new(State::empty(budget.bytes()))) }
    }

    /// The cached member for each of `digests`, in order, touching each
    /// hit's group.
    pub(crate) fn lookup(&self, digests: impl IntoIterator<Item = Digest>) -> Vec<Option<ClosureArtifact>> {
        let mut state = self.lock();
        digests.into_iter().map(|digest| state.hit(digest)).collect()
    }

    /// The cached member stored under `digest`, touching its group.
    pub(crate) fn get(&self, digest: Digest) -> Option<ClosureArtifact> {
        self.lock().hit(digest)
    }

    /// Cache `members` as one group charged their summed payload lengths,
    /// evicting the least recently used groups until it fits. A digest
    /// already cached keeps its entry, and a group that adds no new member,
    /// or is larger than the whole budget, is not cached.
    pub(crate) fn insert(&self, members: Vec<Verified>) {
        let released = self.lock().insert(members);
        // Dropped here, after the guard: the last reference to a slab frees it.
        drop(released);
    }

    /// The cache state. A poisoned lock resets the cache to empty under the
    /// same budget and clears the poison: a panic mid-update may have left
    /// the accounting inconsistent, and an empty cache is always correct.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            *state = State::empty(state.budget_bytes);
            self.state.clear_poison();
            state
        })
    }
}

/// Identifies one cached group.
type GroupId = u64;

/// The members one check-in allocated, with that allocation's charge.
struct Group {
    charge_bytes: u64,
    /// The digests this group owns in [`State::entries`]: those it added,
    /// never one another group already held.
    members: Vec<Digest>,
    tick: u64,
}

/// The cache state behind the lock.
struct State {
    budget_bytes: u64,
    held_bytes: u64,
    next_tick: u64,
    next_group: GroupId,
    entries: HashMap<Digest, (GroupId, ClosureArtifact)>,
    groups: HashMap<GroupId, Group>,
    /// Every group by its last-use tick, oldest first.
    order: BTreeMap<u64, GroupId>,
}

impl State {
    fn empty(budget_bytes: u64) -> Self {
        Self {
            budget_bytes,
            held_bytes: 0,
            next_tick: 0,
            next_group: 0,
            entries: HashMap::new(),
            groups: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    fn hit(&mut self, digest: Digest) -> Option<ClosureArtifact> {
        let (group, artifact) = self.entries.get(&digest).map(|(group, artifact)| (*group, artifact.clone()))?;
        self.touch(group);
        Some(artifact)
    }

    fn touch(&mut self, id: GroupId) {
        let tick = self.tick();
        if let Some(group) = self.groups.get_mut(&id) {
            self.order.remove(&group.tick);
            group.tick = tick;
            self.order.insert(tick, id);
        }
    }

    fn tick(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick += 1;
        tick
    }

    /// Cache `members` as one group and return every member the cache no
    /// longer holds, evicted or refused, for the caller to drop after
    /// unlocking.
    fn insert(&mut self, members: Vec<Verified>) -> Vec<ClosureArtifact> {
        let charge_bytes = members.iter().map(|member| member.artifact().len()).fold(0, u64::saturating_add);
        if self.budget_bytes == 0
            || charge_bytes > self.budget_bytes
            || members.iter().all(|member| self.entries.contains_key(&member.digest()))
        {
            return members.into_iter().map(Verified::into_artifact).collect();
        }

        let mut released = Vec::new();
        while self.held_bytes.saturating_add(charge_bytes) > self.budget_bytes {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            released.extend(self.evict(oldest));
        }

        let id = self.next_group;
        self.next_group += 1;
        let mut owned = Vec::new();
        for member in members {
            match self.entries.entry(member.digest()) {
                Entry::Occupied(_) => released.push(member.into_artifact()),
                Entry::Vacant(vacant) => {
                    owned.push(*vacant.key());
                    vacant.insert((id, member.into_artifact()));
                }
            }
        }

        let tick = self.tick();
        self.held_bytes = self.held_bytes.saturating_add(charge_bytes);
        self.groups.insert(id, Group { charge_bytes, members: owned, tick });
        self.order.insert(tick, id);
        released
    }

    /// Remove group `id` and the entries it owns. Its tick is already out
    /// of [`Self::order`].
    fn evict(&mut self, id: GroupId) -> Vec<ClosureArtifact> {
        let Some(group) = self.groups.remove(&id) else {
            return Vec::new();
        };
        self.held_bytes = self.held_bytes.saturating_sub(group.charge_bytes);
        group.members.iter().filter_map(|digest| self.entries.remove(digest)).map(|(_, artifact)| artifact).collect()
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::artifact_digest;
    use aether_data::{Blob, KindId};

    use super::{ReadCache, ReadCacheBudget};
    use crate::Digest;
    use crate::closure::Verified;

    const KIND: KindId = KindId(1);

    fn member(payload: &[u8]) -> Verified {
        Verified::check(artifact_digest(KIND, payload), KIND, Blob::from(payload.to_vec())).expect("claim matches")
    }

    /// Whether `digest` is cached, read without touching its group.
    fn holds(cache: &ReadCache, digest: Digest) -> bool {
        cache.lock().entries.contains_key(&digest)
    }

    fn held_bytes(cache: &ReadCache) -> u64 {
        cache.lock().held_bytes
    }

    #[test]
    fn eviction_is_by_least_recently_used_group_and_stays_within_budget() {
        let cache = ReadCache::with_budget(ReadCacheBudget::new(16));
        let [a1, a2, b, c, d] =
            [&b"aaa1"[..], b"aaa2", b"bbbbbbbb", b"cccccccc", b"dddddddd"].map(|payload| member(payload).digest());
        cache.insert(vec![member(b"aaa1"), member(b"aaa2")]);
        cache.insert(vec![member(b"bbbbbbbb")]);

        assert!(cache.get(a2).is_some());
        cache.insert(vec![member(b"cccccccc")]);
        assert!(holds(&cache, a1) && holds(&cache, a2) && holds(&cache, c));
        assert!(!holds(&cache, b));
        assert_eq!(held_bytes(&cache), 16);

        cache.insert(vec![member(b"dddddddd")]);
        assert!(!holds(&cache, a1) && !holds(&cache, a2));
        assert!(holds(&cache, c) && holds(&cache, d));
        assert_eq!(held_bytes(&cache), 16);
    }

    #[test]
    fn an_oversized_group_is_not_cached_and_evicts_nothing() {
        let cache = ReadCache::with_budget(ReadCacheBudget::new(8));
        let (kept, oversized) = (member(b"kkkkkkkk").digest(), member(b"ooooooooo").digest());
        cache.insert(vec![member(b"kkkkkkkk")]);
        cache.insert(vec![member(b"ooooooooo")]);

        assert!(holds(&cache, kept));
        assert!(!holds(&cache, oversized));
        assert_eq!(held_bytes(&cache), 8);
    }

    #[test]
    fn a_digest_already_cached_is_not_reinserted_by_a_later_group() {
        let cache = ReadCache::with_budget(ReadCacheBudget::new(12));
        let (shared, only_later) = (member(b"ssss").digest(), member(b"llll").digest());
        cache.insert(vec![member(b"ssss")]);
        cache.insert(vec![member(b"ssss"), member(b"llll")]);

        assert!(cache.get(shared).is_some());
        cache.insert(vec![member(b"nnnn")]);
        assert!(holds(&cache, shared));
        assert!(!holds(&cache, only_later));
    }
}
