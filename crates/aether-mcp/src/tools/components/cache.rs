//! The component-capability cache: what `describe_component` and
//! `send_mail`'s declared-reply lookup read before asking an engine, the
//! forward-model stand-in for the engine's own component registry.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use aether_data::{EngineId, ErasedActorPath};
use aether_kinds::{ComponentCapabilities, PublishedType};

/// Component receive-side capabilities in two maps, behind one mutex:
/// live instances by `(engine, canonical path)` and published types by
/// `(engine, published namespace)`. An instance key is always the canonical
/// lineage the engine answered with, so a short-path spelling and its
/// canonical expansion share one entry. `publish` fills types, `spawn` and
/// `load_component` fill both, and a publish that republishes a live group
/// rewrites every cached instance of the namespaces it bound.
#[derive(Default)]
pub struct ComponentCache {
    maps: Mutex<CacheMaps>,
}

#[derive(Default)]
struct CacheMaps {
    instances: HashMap<(EngineId, ErasedActorPath), ComponentCapabilities>,
    types: HashMap<(EngineId, String), ComponentCapabilities>,
}

impl ComponentCache {
    fn maps(&self) -> MutexGuard<'_, CacheMaps> {
        self.maps.lock().expect("component cache mutex is never poisoned")
    }

    /// Record an instance a spawn answered for, and the published type it
    /// is an instance of.
    pub(in crate::tools) fn record_spawned(
        &self,
        engine: EngineId,
        namespace: &str,
        path: ErasedActorPath,
        capabilities: &ComponentCapabilities,
    ) {
        let mut maps = self.maps();
        maps.types.insert((engine, namespace.to_owned()), capabilities.clone());
        maps.instances.insert((engine, path), capabilities.clone());
    }

    /// Record the types a publish bound. Every cached instance on `engine`
    /// whose leaf namespace one of them names now runs that type's module,
    /// so its cached surface becomes the type's.
    pub(in crate::tools) fn record_published(&self, engine: EngineId, types: &[PublishedType]) {
        let mut maps = self.maps();
        for published in types {
            maps.types.insert((engine, published.namespace.clone()), published.capabilities.clone());
        }
        for ((cached_engine, path), capabilities) in &mut maps.instances {
            if let Some(published) =
                types.iter().find(|published| *cached_engine == engine && published.namespace == leaf_namespace(path))
            {
                capabilities.clone_from(&published.capabilities);
            }
        }
    }

    /// Record one instance's surface, as a live describe answered it.
    pub(in crate::tools) fn record_instance(
        &self,
        engine: EngineId,
        path: ErasedActorPath,
        capabilities: ComponentCapabilities,
    ) {
        self.maps().instances.insert((engine, path), capabilities);
    }

    /// Record one published type's surface, as a live describe answered it.
    pub(in crate::tools) fn record_type(&self, engine: EngineId, namespace: &str, capabilities: ComponentCapabilities) {
        self.maps().types.insert((engine, namespace.to_owned()), capabilities);
    }

    /// The cached surface of the instance at `path` on `engine`.
    pub(in crate::tools) fn instance(&self, engine: EngineId, path: &ErasedActorPath) -> Option<ComponentCapabilities> {
        self.maps().instances.get(&(engine, path.clone())).cloned()
    }

    /// The cached surface of the type published as `namespace` on `engine`.
    pub(in crate::tools) fn published_type(&self, engine: EngineId, namespace: &str) -> Option<ComponentCapabilities> {
        self.maps().types.get(&(engine, namespace.to_owned())).cloned()
    }

    /// Forget the type published as `namespace` and every cached instance
    /// whose leaf namespace matches it, after an unpublish withdrew it. Live
    /// instances are already gone — the host refuses an unpublish while one
    /// still runs — so this clears the type entry and the stale
    /// dropped-instance entries, and a later describe by namespace goes live
    /// to the engine and reports the withdrawal.
    pub(in crate::tools) fn forget_namespace(&self, engine: EngineId, namespace: &str) {
        let mut maps = self.maps();
        maps.types.remove(&(engine, namespace.to_owned()));
        maps.instances.retain(|(cached_engine, path), _| *cached_engine != engine || leaf_namespace(path) != namespace);
    }
}

/// The namespace of the type an actor path names: its last segment, before
/// any `:key` (ADR-0241 §5).
pub(super) fn leaf_namespace(path: &ErasedActorPath) -> &str {
    let leaf = path.as_str().rsplit('/').next().unwrap_or(path.as_str());
    leaf.split_once(':').map_or(leaf, |(namespace, _)| namespace)
}
