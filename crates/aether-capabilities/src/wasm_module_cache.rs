//! Per-chassis cache of compiled Wasmtime modules.
//!
//! Repeated-load `TestBench` scenarios opt into this cache so identical wasm
//! bytes reuse Wasmtime's compiled code. Each chassis boot owns a fresh
//! instance; component stores and actor state are never shared.

use std::sync::{Arc, Mutex, PoisonError};

use wasmtime::{Engine, Module};

struct CachedModule {
    wasm: Box<[u8]>,
    module: Module,
}

/// A thread-safe, exact-byte-keyed collection of compiled Wasmtime modules.
///
/// Test scenarios load only a handful of distinct modules, while their wasm
/// binaries can be tens of megabytes. Keeping one byte copy and comparing it
/// directly avoids running an unoptimized cryptographic hash over every load;
/// exact equality also makes collisions impossible.
pub struct WasmModuleCache {
    engine: Arc<Engine>,
    modules: Mutex<Vec<CachedModule>>,
}

impl WasmModuleCache {
    /// Create an empty cache whose modules all belong to `engine`.
    #[must_use]
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine, modules: Mutex::new(Vec::new()) }
    }

    /// Return the cached module for `wasm`, compiling and retaining it on the
    /// first request.
    pub fn compile(&self, wasm: &[u8]) -> wasmtime::Result<Module> {
        let mut modules = self.modules.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = modules.iter().find(|cached| cached.wasm.as_ref() == wasm) {
            return Ok(cached.module.clone());
        }

        let module = Module::new(&self.engine, wasm)?;
        modules.push(CachedModule { wasm: wasm.into(), module: module.clone() });
        drop(modules);
        Ok(module)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, PoisonError};

    use wasmtime::Engine;

    use super::WasmModuleCache;

    #[test]
    fn identical_bytes_occupy_one_entry() {
        let cache = WasmModuleCache::new(Arc::new(Engine::default()));
        let wasm = b"\0asm\x01\0\0\0";

        cache.compile(wasm).expect("compile first module");
        cache.compile(wasm).expect("reuse first module");

        assert_eq!(cache.modules.lock().unwrap_or_else(PoisonError::into_inner).len(), 1);
    }
}
