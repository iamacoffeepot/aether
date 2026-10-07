//! The fs namespace registry + its boot config. [`AdapterRegistry`]
//! maps a namespace short name (`"save"`, `"assets"`, `"config"`,
//! `"objects"`) to the [`FileAdapter`] backing it; [`NamespaceRoots`]
//! is the ADR-0090 derive-`Config` struct chassis mains resolve at boot
//! and hand to `with_actor::<FsCapability>(roots)`; [`build_registry`]
//! wires the three ADR-0041 path namespaces and the hash-addressed
//! `objects` namespace (ADR-0163 §1) into a populated registry.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use super::adapter::{Access, FileAdapter, LocalFileAdapter};
use super::config::NamespaceRoots;
use super::object_adapter::ObjectAdapter;

/// Namespace → adapter table built at chassis boot. The cap reads
/// `namespace` off an incoming `Read`/`Write`/etc. mail, looks up
/// the adapter here, and either drives the call or replies
/// `FsError::UnknownNamespace`. Registration is one-shot at boot;
/// hot-swap is out of scope.
#[derive(Default)]
pub struct AdapterRegistry {
    adapters: HashMap<String, Arc<dyn FileAdapter>>,
}

impl AdapterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self { adapters: HashMap::new() }
    }

    pub fn register(&mut self, namespace: impl Into<String>, adapter: Arc<dyn FileAdapter>) {
        self.adapters.insert(namespace.into(), adapter);
    }

    pub fn get(&self, namespace: &str) -> Option<Arc<dyn FileAdapter>> {
        self.adapters.get(namespace).map(Arc::clone)
    }
}

/// Populate a fresh `AdapterRegistry` from the supplied
/// [`NamespaceRoots`]: a `LocalFileAdapter` for each of the three
/// ADR-0041 namespaces (`save` and `config` writable, `assets`
/// read-only) and an `ObjectAdapter` for `objects`, which reads one
/// hash-named file per path and refuses every other verb. The three
/// local roots are created and canonicalized here; the `objects` root
/// is neither, so it may not exist. Returns the populated registry
/// along with the roots echoed back (cloned) so the chassis can log
/// what it actually wired.
pub fn build_registry(roots: NamespaceRoots) -> io::Result<(Arc<AdapterRegistry>, NamespaceRoots)> {
    let mut registry = AdapterRegistry::new();
    let save = Arc::new(LocalFileAdapter::new(roots.save.clone(), Access::ReadWrite)?);
    let assets = Arc::new(LocalFileAdapter::new(roots.assets.clone(), Access::ReadOnly)?);
    let config = Arc::new(LocalFileAdapter::new(roots.config.clone(), Access::ReadWrite)?);
    let objects = Arc::new(ObjectAdapter::new(roots.objects.clone()));
    registry.register("save", save as Arc<dyn FileAdapter>);
    registry.register("assets", assets as Arc<dyn FileAdapter>);
    registry.register("config", config as Arc<dyn FileAdapter>);
    registry.register("objects", objects as Arc<dyn FileAdapter>);
    Ok((Arc::new(registry), roots))
}
