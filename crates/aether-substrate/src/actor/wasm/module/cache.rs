//! The engine's one module cache: a [`Module`] per content hash.
//!
//! Cranelift compilation dominates a `LoadComponent`: the widget kit's
//! behavior-host artifact is a 16 MB debug wasm and compiling it takes about a
//! second, while the rest of the load path costs microseconds. Loads arrive in
//! bursts of the *same* bytes: `load_component(replicas: N)` fans one selector
//! into N instances, a boot manifest names several exports of one module, and
//! a scenario that activates every export of an artifact loads it once per
//! export. Without a cache each load past the first recompiled a module the
//! engine had already compiled (iamacoffeepot/aether#5749: an activation
//! scenario over six exports of each of two artifacts ran twelve compiles for
//! two distinct blobs, and intermittently timed out against the test runner's
//! cap because of it).
//!
//! ADR-0240 D5 and ADR-0241 §2: one entry per content hash per engine, kept
//! alive for as long as any holder (a trampoline, an in-flight load or
//! replace, a staged boot plan) still holds a [`Module`] clone, and never
//! evicted by count or capacity. The map keys each hash to a weak reference,
//! so the clones' strong count is the liveness signal, and a hash whose last
//! holder dropped is pruned on the next insert rather than lingering.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use aether_data::{Blob, BlobHash};
use rustc_hash::FxHashMap;
use wasmtime::Engine;

use super::manifest::ModuleManifest;
use super::{Module, ModuleEntry};
use crate::actor::native::BlobCheckIn;

/// Every module a live holder still references, by content hash. A clone is
/// another handle on the same cache.
#[derive(Clone)]
pub struct ModuleCache {
    shared: Arc<Shared>,
}

struct Shared {
    engine: Arc<Engine>,
    entries: Mutex<FxHashMap<BlobHash, Weak<ModuleEntry>>>,
}

impl ModuleCache {
    /// An empty cache that compiles on `engine`.
    #[must_use]
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { shared: Arc::new(Shared { engine, entries: Mutex::new(FxHashMap::default()) }) }
    }

    /// The module for `code`'s bytes.
    ///
    /// Returns the live module for the bytes' hash when one exists. Otherwise
    /// it parses the manifest first, so a section it cannot read refuses
    /// before any compile time is spent, then compiles. `code` is read where
    /// it already sits in the store, or checked in once when it is `Owned`,
    /// and is never kept, nor is any asset section's payload: the bytes
    /// leave the store when the caller drops `code` (ADR-0163 §3).
    ///
    /// # Errors
    ///
    /// The section reader's error for a section it cannot read, or
    /// `invalid wasm module: …` for bytes that do not compile.
    pub fn check_in(&self, blobs: &BlobCheckIn, code: &Blob) -> Result<Module, String> {
        let stored = blobs.entry(code).map_err(|error| format!("invalid wasm module: {error}"))?;
        let hash = stored.hash();

        let live = self.lock().get(&hash).and_then(Weak::upgrade);
        if let Some(entry) = live {
            return Ok(Module { entry });
        }

        let manifest = ModuleManifest::parse(stored.bytes())?;
        let compiled = wasmtime::Module::new(&self.shared.engine, stored.bytes())
            .map_err(|error| format!("invalid wasm module: {error}"))?;
        let fresh = Arc::new(ModuleEntry { hash, compiled, manifest });

        // A concurrent check-in of the same hash that finished first wins, so
        // one hash never answers two entries; the losing entry drops once the
        // lock is released.
        let winner = {
            let mut entries = self.lock();
            entries.retain(|_, entry| entry.strong_count() > 0);
            let winner = entries.get(&hash).and_then(Weak::upgrade);
            if winner.is_none() {
                entries.insert(hash, Arc::downgrade(&fresh));
            }
            winner
        };
        Ok(Module { entry: winner.unwrap_or(fresh) })
    }

    fn lock(&self) -> MutexGuard<'_, FxHashMap<BlobHash, Weak<ModuleEntry>>> {
        // No map operation leaves it half-updated, so a poisoned lock still
        // guards a consistent map.
        self.shared.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.lock().len()
    }
}
