//! The fs namespace registry + its boot config. [`AdapterRegistry`]
//! maps a namespace short name (`"save"`, `"assets"`, `"config"`) to
//! the [`FileAdapter`](super::FileAdapter) backing it; [`NamespaceRoots`]
//! is the ADR-0090 derive-`Config` struct chassis mains resolve at boot
//! and hand to `with_actor::<FsCapability>(roots)`; [`build_registry`]
//! wires the three ADR-0041 namespaces into a populated registry.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use super::adapter::{FileAdapter, LocalFileAdapter};
use super::config::NamespaceRoots;

/// Namespace → adapter table built at chassis boot. The cap reads
/// `namespace` off an incoming `Read`/`Write`/etc. mail, looks up
/// the adapter here, and either drives the call or replies
/// `FsError::UnknownNamespace`. Registration is one-shot at boot;
/// hot-swap is out of scope.
pub struct AdapterRegistry {
    adapters: HashMap<String, Arc<dyn FileAdapter>>,
}

impl AdapterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }

    pub fn register(&mut self, namespace: impl Into<String>, adapter: Arc<dyn FileAdapter>) {
        self.adapters.insert(namespace.into(), adapter);
    }

    pub fn get(&self, namespace: &str) -> Option<Arc<dyn FileAdapter>> {
        self.adapters.get(namespace).map(Arc::clone)
    }

    #[must_use]
    pub fn has(&self, namespace: &str) -> bool {
        self.adapters.contains_key(namespace)
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Populate a fresh `AdapterRegistry` with `LocalFileAdapter`s for
/// each of the three ADR-0041 namespaces using the supplied
/// [`NamespaceRoots`]. `save` and `config` are writable; `assets` is
/// read-only. Returns the populated registry along with the roots
/// echoed back (cloned) so the chassis can log what it actually
/// wired.
pub fn build_registry(roots: NamespaceRoots) -> io::Result<(Arc<AdapterRegistry>, NamespaceRoots)> {
    let mut registry = AdapterRegistry::new();
    let save = Arc::new(LocalFileAdapter::new(roots.save.clone(), true)?);
    let assets = Arc::new(LocalFileAdapter::new(roots.assets.clone(), false)?);
    let config = Arc::new(LocalFileAdapter::new(roots.config.clone(), true)?);
    registry.register("save", save as Arc<dyn FileAdapter>);
    registry.register("assets", assets as Arc<dyn FileAdapter>);
    registry.register("config", config as Arc<dyn FileAdapter>);
    Ok((Arc::new(registry), roots))
}
