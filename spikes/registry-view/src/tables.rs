//! The three routing-table shapes under comparison. Each mirrors the
//! production `Registry` value shape — a display name plus an `Arc`'d
//! handler — so clone-out costs match what `route_lookup` pays today.

use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use rustc_hash::FxHashMap;

pub type Handler = dyn Fn(u64) -> u64 + Send + Sync;

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub handler: Arc<Handler>,
}

impl Entry {
    pub fn new(id: u64) -> Self {
        Self { name: format!("aether.tcp/session:conn-{id}"), handler: Arc::new(move |x| x ^ id) }
    }
}

/// The clone-out a `route_lookup` performs today: the kind/display name
/// plus the handler `Arc`, owned, so the caller can drop the guard (or
/// snapshot) before dispatching.
pub type RouteClone = (String, Arc<Handler>);

fn clone_out(entry: &Entry) -> RouteClone {
    (entry.name.clone(), Arc::clone(&entry.handler))
}

/// Production shape: one `std::sync::RwLock` over an `FxHashMap`, reads
/// take a read guard and clone out, writes take the write guard per
/// operation from whichever thread is spawning.
pub struct LockTable {
    inner: RwLock<FxHashMap<u64, Entry>>,
}

impl LockTable {
    pub fn seeded(n: u64) -> Self {
        Self { inner: RwLock::new(seed_fx(n)) }
    }

    pub fn read(&self, id: u64) -> Option<RouteClone> {
        self.inner.read().unwrap().get(&id).map(clone_out)
    }

    pub fn insert(&self, id: u64, entry: Entry) {
        self.inner.write().unwrap().insert(id, entry);
    }

    pub fn remove(&self, id: u64) {
        self.inner.write().unwrap().remove(&id);
    }
}

/// Proposed shape, clone-per-batch publish strategy: readers load an
/// `ArcSwap` snapshot (plain atomic load, wait-free); the single owner
/// mutates a private working map and republishes a full clone once per
/// applied batch.
pub struct SwapTable {
    current: ArcSwap<FxHashMap<u64, Entry>>,
}

impl SwapTable {
    pub fn seeded(n: u64) -> Self {
        Self { current: ArcSwap::from_pointee(seed_fx(n)) }
    }

    pub fn read(&self, id: u64) -> Option<RouteClone> {
        self.current.load().get(&id).map(clone_out)
    }

    /// The mode the snapshot design enables and the lock forbids: use the
    /// entry in place under the (cheap, wait-free) snapshot guard instead
    /// of cloning out. Production's clone-out exists only because holding
    /// an `RwLock` read guard across a handler is unacceptable.
    pub fn read_in_place(&self, id: u64) -> Option<u64> {
        self.current.load().get(&id).map(|e| (e.handler)(id))
    }

    pub fn publish(&self, next: FxHashMap<u64, Entry>) {
        self.current.store(Arc::new(next));
    }

    /// Install `next` as the head and hand back the previous head — the
    /// double-buffer strategy's role swap. The returned map becomes the
    /// standby the owner replays the lag onto next cycle.
    pub fn swap_in(&self, next: Arc<FxHashMap<u64, Entry>>) -> Arc<FxHashMap<u64, Entry>> {
        self.current.swap(next)
    }
}

/// Proposed shape, structural-sharing publish strategy: same `ArcSwap`
/// read side over an `im::HashMap`, so the owner can republish per
/// operation at O(log n) instead of cloning the whole map.
pub struct ImTable {
    current: ArcSwap<im::HashMap<u64, Entry>>,
}

impl ImTable {
    pub fn seeded(n: u64) -> Self {
        let mut map = im::HashMap::new();
        for id in 0..n {
            map.insert(id, Entry::new(id));
        }
        Self { current: ArcSwap::from_pointee(map) }
    }

    pub fn read(&self, id: u64) -> Option<RouteClone> {
        self.current.load().get(&id).map(clone_out)
    }

    pub fn snapshot(&self) -> im::HashMap<u64, Entry> {
        im::HashMap::clone(&self.current.load())
    }

    pub fn publish(&self, next: im::HashMap<u64, Entry>) {
        self.current.store(Arc::new(next));
    }
}

pub fn seed_fx(n: u64) -> FxHashMap<u64, Entry> {
    (0..n).map(|id| (id, Entry::new(id))).collect()
}

/// Deterministic id stream (64-bit LCG), one instance per thread so
/// readers don't share state.
pub struct Ids {
    state: u64,
    modulus: u64,
}

impl Ids {
    pub fn new(seed: u64, modulus: u64) -> Self {
        Self { state: seed.wrapping_mul(2862933555777941757).wrapping_add(3037000493), modulus }
    }

    pub fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.state >> 33) % self.modulus
    }
}
