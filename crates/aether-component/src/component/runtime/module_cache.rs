//! Compiled-module reuse across loads of identical wasm bytes.
//!
//! Cranelift compilation dominates a `LoadComponent`: the widget kit's
//! behavior-host artifact is a 16 MB debug wasm and `Module::new` spends
//! ~1 s on it, while the rest of the load path costs microseconds. Loads
//! arrive in bursts of the *same* bytes — `load_component(replicas: N)`
//! fans one selector into N instances, a boot manifest names several
//! exports of one module, and a scenario that activates every export of an
//! artifact loads it once per export — so before this cache each load past
//! the first recompiled a module the host had already compiled
//! (iamacoffeepot/aether#5749: an activation scenario over six exports of
//! each of two artifacts ran twelve compiles for two distinct blobs, and
//! intermittently timed out against the test runner's cap because of it).
//!
//! ADR-0240 D5: one compiled module per content hash per engine, kept alive
//! for as long as any holder — a trampoline, an in-flight load, a staged
//! boot plan — still references it, and never evicted by count or
//! capacity. `wasmtime::Module` has no public weak handle, so the map keys
//! each hash to a [`Weak<Module>`] and every holder keeps an `Arc<Module>`
//! instead; the `Arc`'s strong count is the liveness signal, and a hash
//! whose last holder dropped is pruned from the map on the next `compile`
//! rather than lingering.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use wasmtime::{Engine, Module};

/// Compiled modules currently held by at least one live user, addressed by
/// content hash.
#[derive(Default)]
pub struct ModuleCache {
    modules: HashMap<String, Weak<Module>>,
}

impl ModuleCache {
    /// The compiled module for `wasm`, whose sha256 content hash is `hash`.
    ///
    /// Returns the still-live module for `hash` when one exists and compiles
    /// on `engine` otherwise, recording the fresh module's weak reference.
    /// Callers must pass the hash of the bytes they pass: the hash is the
    /// whole identity here, so a mismatched pair would hand back the wrong
    /// component's code.
    pub fn compile(&mut self, engine: &Engine, hash: &str, wasm: &[u8]) -> Result<Arc<Module>, wasmtime::Error> {
        self.modules.retain(|_, module| module.strong_count() > 0);

        if let Some(module) = self.modules.get(hash).and_then(Weak::upgrade) {
            return Ok(module);
        }

        let module = Arc::new(Module::new(engine, wasm)?);
        self.modules.insert(hash.to_owned(), Arc::downgrade(&module));
        Ok(module)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPHA: &[u8] = br#"(module (func (export "alpha")))"#;
    const BETA: &[u8] = br#"(module (func (export "beta")))"#;

    /// Tripwire: every hash currently held live must answer with its own
    /// compiled code, even when another hash's compile interleaves in
    /// between. A key that collapsed two artifacts together — keyed on byte
    /// length, say — would hand a load the wrong module's compiled code and
    /// instantiate the wrong component under the requested name, with no
    /// error anywhere on the load path. A regression to one-slot behaviour
    /// — where holding `alpha` while `beta` compiles evicts and forces
    /// `alpha` to recompile on its next call — would break the `Arc::ptr_eq`
    /// checks below rather than merely costing time, so this catches it too.
    #[test]
    fn every_live_module_is_answered_for_its_own_content_hash() {
        let engine = Engine::default();
        let mut cache = ModuleCache::default();

        let alpha = cache.compile(&engine, "alpha-hash", ALPHA).expect("compile alpha");
        let beta = cache.compile(&engine, "beta-hash", BETA).expect("compile beta");

        let alpha_again = cache.compile(&engine, "alpha-hash", ALPHA).expect("recompile alpha");
        let beta_again = cache.compile(&engine, "beta-hash", BETA).expect("recompile beta");

        assert!(Arc::ptr_eq(&alpha, &alpha_again), "alpha's hash must answer with alpha's own compiled module");
        assert!(Arc::ptr_eq(&beta, &beta_again), "beta's hash must answer with beta's own compiled module");
        assert!(alpha.get_export("alpha").is_some(), "alpha's hash must expose alpha's own export");
        assert!(beta.get_export("beta").is_some(), "beta's hash must expose beta's own export");
    }

    /// A hash whose last holder drops must leave the map — never evicted by
    /// count or capacity (ADR-0240 D5), but also never left to accumulate
    /// once nothing references it — and a later compile of that same hash
    /// must recompile rather than upgrading a dead weak reference or
    /// otherwise failing.
    #[test]
    fn a_module_leaves_the_map_once_its_last_holder_drops() {
        let engine = Engine::default();
        let mut cache = ModuleCache::default();

        let alpha = cache.compile(&engine, "alpha-hash", ALPHA).expect("compile alpha");
        let weak_alpha = Arc::downgrade(&alpha);
        drop(alpha);
        assert!(weak_alpha.upgrade().is_none(), "the map must hold no strong reference of its own");

        let _beta = cache.compile(&engine, "beta-hash", BETA).expect("compile beta");
        assert_eq!(cache.modules.len(), 1, "the freed alpha entry is pruned rather than left to accumulate");

        let alpha_again = cache.compile(&engine, "alpha-hash", ALPHA).expect("recompile alpha");
        assert!(alpha_again.get_export("alpha").is_some(), "a dead entry recompiles rather than failing");
    }
}
