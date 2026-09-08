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
//! One slot, keyed by the sha256 content hash the module-boot registry
//! already keys on. One slot is what a burst needs, and it bounds what the
//! host retains to a single compiled artifact: every live trampoline holds
//! its own `Module` clone, so the slot only ever keeps alive a module that
//! would otherwise have been dropped.

use wasmtime::{Engine, Module};

/// The host's most recently compiled module, addressed by content hash.
#[derive(Default)]
pub struct ModuleCache {
    cached: Option<CachedModule>,
}

struct CachedModule {
    hash: String,
    module: Module,
}

impl ModuleCache {
    /// The compiled module for `wasm`, whose sha256 content hash is `hash`.
    ///
    /// Returns the cached module when `hash` matches the slot and compiles
    /// on `engine` otherwise, taking the slot. Callers must pass the hash
    /// of the bytes they pass: the hash is the whole identity here, so a
    /// mismatched pair would hand back the wrong component's code.
    pub fn compile(&mut self, engine: &Engine, hash: &str, wasm: &[u8]) -> Result<Module, wasmtime::Error> {
        if let Some(cached) = &self.cached
            && cached.hash == hash
        {
            return Ok(cached.module.clone());
        }

        let module = Module::new(engine, wasm)?;
        self.cached = Some(CachedModule { hash: hash.to_owned(), module: module.clone() });
        Ok(module)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the slot must answer on content identity alone. A key that
    /// collapsed two artifacts together — keyed on byte length, or a slot
    /// that answered any hit after the first — would hand a load the wrong
    /// module's compiled code and instantiate the wrong component under the
    /// requested name, with no error anywhere on the load path. The
    /// interleave is the shape that catches it: `a`, then `b`, then `a`
    /// again, where the third call must recompile rather than return the
    /// `b` still in the slot.
    #[test]
    fn a_module_is_only_ever_answered_for_its_own_content_hash() {
        let engine = Engine::default();
        let mut cache = ModuleCache::default();
        // wasmtime's `wat` feature accepts the text form straight through
        // `Module::new`, so the two artifacts need no fixture build.
        let alpha = br#"(module (func (export "alpha")))"#;
        let beta = br#"(module (func (export "beta")))"#;

        let first = cache.compile(&engine, "alpha-hash", alpha).expect("compile alpha");
        assert!(first.get_export("alpha").is_some(), "the first compile answers with the bytes it was handed");

        let switched = cache.compile(&engine, "beta-hash", beta).expect("compile beta");
        assert!(switched.get_export("beta").is_some(), "a fresh hash compiles the new bytes rather than reusing");

        let returned = cache.compile(&engine, "alpha-hash", alpha).expect("recompile alpha");
        assert!(returned.get_export("alpha").is_some(), "an evicted hash recompiles rather than answering with beta");

        let hit = cache.compile(&engine, "alpha-hash", alpha).expect("cached alpha");
        assert!(hit.get_export("alpha").is_some(), "a repeat of the slot's own hash still answers with alpha");
    }
}
