//! The publication table (ADR-0241 §3): the engine's record of which code
//! implements each namespace it publishes.
//!
//! Native code publishes when the registry is built: every native actor
//! namespace the binary links is [`Published::Native`], and its code is the
//! binary. A native namespace records each linked type that declares it,
//! since several types may share one on purpose and a chassis composes
//! exactly one of them, and which of them this engine has born there: the
//! first birth holds the namespace for the engine's life, and a birth of any
//! other type there is refused ([`PublicationTable::hold`]).
//!
//! A module publishes as one set through the registry owner's `PublishModule`
//! effect, which runs [`admission`] against the table as its batch has staged
//! it and points every namespace the module exports at the module. A
//! namespace, once published, stays published, and a published module stays
//! resident for the engine's life.
//!
//! The table lives on the registry owner's `Inner`, beside the route
//! contracts it already applies. It is not an actor and has no address.

use std::any::{TypeId, type_name};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use aether_data::BlobHash;
use aether_data::name_inventory::native_type_entries;
use rustc_hash::FxHashMap;

#[cfg(feature = "wasm")]
use crate::actor::wasm::module::Module;

use super::address::native_cardinality_facts;

#[cfg(feature = "wasm")]
mod admission;
#[cfg(all(test, feature = "wasm"))]
mod tests;

#[cfg(feature = "wasm")]
pub use admission::AdmissionRefusal;
#[cfg(feature = "wasm")]
pub(super) use admission::{Admitted, ModuleSurface, admit};

#[cfg(feature = "wasm")]
use admission::Holder;

/// One native actor type: its `TypeId` and its name, for refusals.
#[derive(Clone, Copy, Debug)]
pub struct NativeType {
    pub(super) id: TypeId,
    pub(super) name: &'static str,
}

impl NativeType {
    /// The native type `A`.
    pub fn of<A: 'static>() -> Self {
        Self { id: TypeId::of::<A>(), name: type_name::<A>() }
    }
}

/// What implements one published namespace.
#[derive(Clone)]
pub(super) enum Published {
    /// Native actors linked into the binary. Their rows stand on every route
    /// they publish at birth, so the table records only the linked types and
    /// which of them this engine has born.
    Native(NativePublication),
    /// A published module, shared by every namespace it exports.
    #[cfg(feature = "wasm")]
    Module(Arc<ModulePublication>),
}

impl Published {
    /// The hash of the module that implements the namespace, or `None` for
    /// native code.
    fn module_hash(&self) -> Option<BlobHash> {
        match self {
            Self::Native(_) => None,
            #[cfg(feature = "wasm")]
            Self::Module(publication) => Some(publication.module.hash()),
        }
    }
}

/// A native namespace: the linked types that declare it, and the one this
/// engine has born there, once it has.
#[derive(Clone, Default)]
pub(super) struct NativePublication {
    /// Every linked type the native `#[actor]` derive declared here. Empty
    /// for a namespace only hand-written `Addressable` impls declare.
    types: Vec<NativeType>,
    /// The type this engine first bore here. Never released.
    held: Option<NativeType>,
}

/// One published module and the surface admission compared it by.
#[cfg(feature = "wasm")]
pub(super) struct ModulePublication {
    module: Module,
    surface: ModuleSurface,
}

/// Every published namespace and what implements it. A clone shares each
/// module publication.
#[derive(Clone)]
pub(super) struct PublicationTable {
    namespaces: FxHashMap<Arc<str>, Published>,
}

/// Why a native birth was refused its namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeHoldRefusal {
    /// Another type sharing the namespace was born here first.
    HeldByOther { namespace: &'static str, holder: &'static str, type_name: &'static str },
    /// The type is not among the linked types that declare the namespace: a
    /// hand-written `Addressable` impl reusing a derived or module namespace.
    NotLinked { namespace: &'static str, type_name: &'static str },
}

impl fmt::Display for NativeHoldRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeldByOther { namespace, holder, type_name } => write!(
                formatter,
                "{type_name} cannot be born at {namespace}: this engine already bore {holder} there \
                 (a shared native namespace is selected by composing one of its types)"
            ),
            Self::NotLinked { namespace, type_name } => write!(
                formatter,
                "{type_name} cannot be born at {namespace}: it is not among the types this binary links there"
            ),
        }
    }
}

impl Error for NativeHoldRefusal {}

impl PublicationTable {
    /// The native publications: every native actor namespace this binary
    /// links, read from the same link-time facts the address index reads,
    /// each with the linked types that declare it and no hold.
    pub(super) fn native() -> Self {
        let mut native: FxHashMap<Arc<str>, NativePublication> = native_cardinality_facts()
            .map(|(namespace, _)| (Arc::from(namespace), NativePublication::default()))
            .collect();
        for entry in native_type_entries() {
            let linked = NativeType { id: (entry.type_id)(), name: (entry.type_name)() };
            let types = &mut native.entry(Arc::from(entry.namespace)).or_default().types;
            if !types.iter().any(|known| known.id == linked.id) {
                types.push(linked);
            }
        }
        Self {
            namespaces: native.into_iter().map(|(namespace, native)| (namespace, Published::Native(native))).collect(),
        }
    }

    /// Hold `namespace` for the native type `born`, admitting its birth when
    /// the namespace is unheld or already held by `born`. A namespace the
    /// table has never heard of belongs to a hand-written `Addressable` impl
    /// (#6870), and its first birth records it. The hold is never released.
    pub(super) fn hold(&mut self, namespace: &'static str, born: NativeType) -> Result<(), NativeHoldRefusal> {
        let Some(published) = self.namespaces.get_mut(namespace) else {
            self.namespaces.insert(
                Arc::from(namespace),
                Published::Native(NativePublication { types: Vec::new(), held: Some(born) }),
            );
            return Ok(());
        };
        let publication = match published {
            Published::Native(publication) => publication,
            #[cfg(feature = "wasm")]
            Published::Module(_) => return Err(NativeHoldRefusal::NotLinked { namespace, type_name: born.name }),
        };
        match publication.held {
            Some(holder) if holder.id == born.id => Ok(()),
            Some(holder) => {
                Err(NativeHoldRefusal::HeldByOther { namespace, holder: holder.name, type_name: born.name })
            }
            None if !publication.types.is_empty() && !publication.types.iter().any(|linked| linked.id == born.id) => {
                Err(NativeHoldRefusal::NotLinked { namespace, type_name: born.name })
            }
            None => {
                publication.held = Some(born);
                Ok(())
            }
        }
    }

    /// Whether `namespace` is published by the module `module`, the one check
    /// a guest birth passes before the owner reserves it (ADR-0241 §3, §6):
    /// a native, unpublished, or other module's namespace binds no guest.
    pub(super) fn binds(&self, namespace: &str, module: BlobHash) -> bool {
        self.namespaces.get(namespace).and_then(Published::module_hash) == Some(module)
    }

    /// Whether a published module implements `namespace`, so an actor named
    /// by it is a guest (ADR-0241 §3): the table, not the name's spelling,
    /// says which routes a module's code runs behind.
    pub(super) fn is_module(&self, namespace: &str) -> bool {
        self.namespaces.get(namespace).and_then(Published::module_hash).is_some()
    }

    /// Who holds `namespace`, or `None` when it is unpublished.
    #[cfg(feature = "wasm")]
    pub(super) fn holder(&self, namespace: &str) -> Option<Holder<'_>> {
        self.namespaces.get(namespace).map(|published| match published {
            Published::Native(_) => Holder::Native,
            Published::Module(publication) => {
                Holder::Module { hash: publication.module.hash(), surface: &publication.surface }
            }
        })
    }

    /// Point every namespace `surface` exports at `module`. Called only by the
    /// registry owner's publish arm, after [`admit`] accepted the module.
    #[cfg(feature = "wasm")]
    pub(super) fn publish(&mut self, module: Module, surface: ModuleSurface) {
        let publication = Arc::new(ModulePublication { module, surface });
        for namespace in publication.surface.exported_namespaces() {
            self.namespaces.insert(Arc::clone(namespace), Published::Module(Arc::clone(&publication)));
        }
    }

    /// The linked types and the hold at a native `namespace`, for tests.
    #[cfg(all(test, feature = "wasm"))]
    pub(super) fn native_publication(&self, namespace: &str) -> Option<(Vec<TypeId>, Option<TypeId>)> {
        match self.namespaces.get(namespace)? {
            Published::Native(publication) => Some((
                publication.types.iter().map(|linked| linked.id).collect(),
                publication.held.map(|holder| holder.id),
            )),
            #[cfg(feature = "wasm")]
            Published::Module(_) => None,
        }
    }
}
