//! The fs namespace registry + its boot config. [`AdapterRegistry`]
//! maps a namespace short name (`"save"`, `"assets"`, `"config"`,
//! `"objects"`) to the [`FileAdapter`] backing it; [`NamespaceRoots`]
//! is the ADR-0090 derive-`Config` struct chassis mains resolve at boot
//! and hand to `with_actor::<FsCapability>(roots)`; [`build_registry`]
//! wires the three ADR-0041 path namespaces and the read-only `objects`
//! namespace (ADR-0163 §1), whose objects an [`ObjectSource`] locates,
//! into a populated registry.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::Arc;

use super::adapter::{Access, FileAdapter, LocalFileAdapter};
use super::config::NamespaceRoots;
use super::object_adapter::{NamedObjectError, ObjectAdapter, ObjectSource};

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

/// Why the namespace registry could not be built.
#[derive(Debug)]
pub enum RegistryError {
    /// A `save`, `assets`, or `config` root could not be created or
    /// canonicalized.
    Root(io::Error),
    /// A named object the package lists is not in its object store as
    /// recorded.
    NamedObject(NamedObjectError),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(source) => write!(f, "prepare a file namespace root: {source}"),
            Self::NamedObject(source) => write!(f, "{source}"),
        }
    }
}

impl Error for RegistryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Root(source) => Some(source),
            Self::NamedObject(source) => Some(source),
        }
    }
}

/// Populate a fresh `AdapterRegistry` from the supplied
/// [`NamespaceRoots`] and [`ObjectSource`]: a `LocalFileAdapter` for each
/// of the three ADR-0041 namespaces (`save` and `config` writable,
/// `assets` read-only) and an `ObjectAdapter` for `objects`, which reads
/// an object by path and refuses `write` and `delete`. The three local
/// roots are created and canonicalized here; `roots.objects` is neither,
/// so it may not exist, and it is read only by the directory source. A
/// package source is checked here: every named object must be present at
/// its recorded length. Returns the populated registry along with the
/// roots echoed back so the chassis can log what it actually wired.
///
/// # Errors
///
/// [`RegistryError::Root`] when a local root is unusable, and
/// [`RegistryError::NamedObject`] when a package's named object is absent
/// or the wrong length.
pub fn build_registry(
    roots: NamespaceRoots,
    objects: ObjectSource,
) -> Result<(Arc<AdapterRegistry>, NamespaceRoots), RegistryError> {
    let mut registry = AdapterRegistry::new();
    let save = Arc::new(LocalFileAdapter::new(roots.save.clone(), Access::ReadWrite).map_err(RegistryError::Root)?);
    let assets = Arc::new(LocalFileAdapter::new(roots.assets.clone(), Access::ReadOnly).map_err(RegistryError::Root)?);
    let config = Arc::new(LocalFileAdapter::new(roots.config.clone(), Access::ReadWrite).map_err(RegistryError::Root)?);
    let objects = Arc::new(ObjectAdapter::new(roots.objects.clone(), objects).map_err(RegistryError::NamedObject)?);
    registry.register("save", save as Arc<dyn FileAdapter>);
    registry.register("assets", assets as Arc<dyn FileAdapter>);
    registry.register("config", config as Arc<dyn FileAdapter>);
    registry.register("objects", objects as Arc<dyn FileAdapter>);
    Ok((Arc::new(registry), roots))
}
