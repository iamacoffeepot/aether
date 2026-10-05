//! The engine's one module cache: a [`Module`] per module file, over one
//! compile per code.
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
//! The cache keeps two maps, each keyed by a BLAKE3 hash:
//!
//! - **Entries, by the hash of the whole module file.** That hash is the
//!   module's identity (ADR-0241 §2): its publication name when it is
//!   content-addressed, its boot-once key, and what a load reports. An entry
//!   holds the file's parsed manifest, asset catalog included, so two files
//!   are two entries whatever they share.
//! - **Compiled code, by the hash of the file's code**: the file with every
//!   asset section removed ([`code`]). The compiler never reads
//!   an asset section, so asset bundles packed from one build have one code
//!   and share one compile, where keying the compile by the file would run
//!   and keep one per bundle (iamacoffeepot/aether#7392). The bytes compiled
//!   are the bytes hashed, so a hit never answers code compiled from other
//!   input. A file with no asset section is its own code, and its code hash
//!   is its file hash.
//!
//! ADR-0240 D5 and ADR-0241 §2: nothing is evicted by count or capacity. An
//! entry is kept alive for as long as any holder (a trampoline, an in-flight
//! load or replace, a staged boot plan) still holds a [`Module`] clone, and
//! compiled code for as long as any entry over it is. Each map keys its hash
//! to a weak reference, so the strong count is the liveness signal, and a
//! hash whose last holder dropped is pruned on the next insert rather than
//! lingering. The two maps are never locked together.

use std::borrow::Cow;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use aether_data::{Blob, BlobHash};
use rustc_hash::FxHashMap;
use wasmtime::Engine;

use super::manifest::ModuleManifest;
use super::{Module, ModuleEntry, code};
use crate::actor::native::BlobCheckIn;

/// Every module a live holder still references, by the hash of its file,
/// and the compiled code those modules share. A clone is another handle on
/// the same cache.
#[derive(Clone)]
pub struct ModuleCache {
    shared: Arc<Shared>,
}

struct Shared {
    engine: Arc<Engine>,
    entries: LiveByHash<ModuleEntry>,
    compiled: LiveByHash<CompiledCode>,
}

/// One compile: the `wasmtime::Module` made from one code, shared by every
/// module whose file carries that code. Built only by
/// [`ModuleCache::check_in`].
pub(super) struct CompiledCode {
    module: wasmtime::Module,
}

impl CompiledCode {
    pub(super) fn module(&self) -> &wasmtime::Module {
        &self.module
    }
}

impl ModuleCache {
    /// An empty cache that compiles on `engine`.
    #[must_use]
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { shared: Arc::new(Shared { engine, entries: LiveByHash::new(), compiled: LiveByHash::new() }) }
    }

    /// The module for `code`'s bytes.
    ///
    /// Returns the live module for the bytes' hash when one exists. Otherwise
    /// it parses the manifest first, so a section it cannot read refuses
    /// before any compile time is spent, then takes the compile of the
    /// bytes' code, compiling only when no live module shares it. `code` is
    /// read where it already sits in the store, or checked in once when it
    /// is `Owned`, and is never kept, nor is any asset section's payload:
    /// the bytes leave the store when the caller drops `code` (ADR-0163 §3).
    ///
    /// # Errors
    ///
    /// The section reader's error for a section it cannot read, or
    /// `invalid wasm module: …` for bytes that do not compile.
    pub fn check_in(&self, blobs: &BlobCheckIn, code: &Blob) -> Result<Module, String> {
        let stored = blobs.entry(code).map_err(|error| format!("invalid wasm module: {error}"))?;
        let hash = stored.hash();

        if let Some(entry) = self.shared.entries.live(hash) {
            return Ok(Module { entry });
        }

        let manifest = ModuleManifest::parse(stored.bytes())?;
        let code = self.compiled_code(hash, stored.bytes())?;
        let fresh = Arc::new(ModuleEntry { hash, code, manifest });

        Ok(Module { entry: self.shared.entries.keep(hash, fresh) })
    }

    /// The compile of `file`'s code: the live one when a module over the
    /// same code is held, otherwise a fresh compile of exactly the bytes its
    /// key hashes.
    fn compiled_code(&self, file_hash: BlobHash, file: &[u8]) -> Result<Arc<CompiledCode>, String> {
        let bytes = code::code_bytes(file)?;
        // A file with no asset section is its own code, so the hash already
        // taken of the file is the hash of its code.
        let hash = match &bytes {
            Cow::Borrowed(_) => file_hash,
            Cow::Owned(stripped) => BlobHash::from_bytes(*blake3::hash(stripped).as_bytes()),
        };

        if let Some(compiled) = self.shared.compiled.live(hash) {
            return Ok(compiled);
        }

        let module = wasmtime::Module::new(&self.shared.engine, &bytes)
            .map_err(|error| format!("invalid wasm module: {error}"))?;
        Ok(self.shared.compiled.keep(hash, Arc::new(CompiledCode { module })))
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.shared.entries.len()
    }

    #[cfg(test)]
    pub(super) fn compiled_len(&self) -> usize {
        self.shared.compiled.len()
    }
}

/// The values a holder still references, by hash. The map holds weak
/// references only, so a value's own holders decide how long it lives.
struct LiveByHash<T> {
    slots: Mutex<FxHashMap<BlobHash, Weak<T>>>,
}

impl<T> LiveByHash<T> {
    fn new() -> Self {
        Self { slots: Mutex::new(FxHashMap::default()) }
    }

    /// The value held live under `hash`.
    fn live(&self, hash: BlobHash) -> Option<Arc<T>> {
        self.lock().get(&hash).and_then(Weak::upgrade)
    }

    /// The value `hash` answers from now on: `fresh`, unless a concurrent
    /// caller that finished first already put one there, so one hash never
    /// answers two values; a losing `fresh` drops once the lock is released.
    /// Dead slots are pruned here.
    fn keep(&self, hash: BlobHash, fresh: Arc<T>) -> Arc<T> {
        let winner = {
            let mut slots = self.lock();
            slots.retain(|_, slot| slot.strong_count() > 0);
            let winner = slots.get(&hash).and_then(Weak::upgrade);
            if winner.is_none() {
                slots.insert(hash, Arc::downgrade(&fresh));
            }
            winner
        };
        winner.unwrap_or(fresh)
    }

    fn lock(&self) -> MutexGuard<'_, FxHashMap<BlobHash, Weak<T>>> {
        // No map operation leaves it half-updated, so a poisoned lock still
        // guards a consistent map.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}
