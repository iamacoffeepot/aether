//! The publication table (ADR-0241 §3): the engine's record of which code
//! implements each namespace it publishes.
//!
//! Native code publishes when the registry is built: every native actor
//! namespace the binary links is [`Published::Native`], and its code is the
//! binary. A module publishes as one set through the registry owner's
//! `PublishModule` effect, which runs [`admission`] against the table as its
//! batch has staged it and points every namespace the module exports at the
//! module. A namespace, once published, stays published, and a published
//! module stays resident for the engine's life.
//!
//! The table lives on the registry owner's `Inner`, beside the route
//! contracts it already applies. It is not an actor and has no address.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::actor::wasm::module::Module;

use super::address::native_cardinality_facts;

mod admission;
#[cfg(test)]
mod tests;

pub use admission::AdmissionRefusal;
pub(super) use admission::{Admitted, ModuleSurface, admit};

use admission::Holder;

/// What implements one published namespace.
#[derive(Clone)]
pub(super) enum Published {
    /// A native actor linked into the binary. Its rows stand on every route it
    /// publishes at birth, so the table records only the namespace.
    Native,
    /// A published module, shared by every namespace it exports.
    Module(Arc<ModulePublication>),
}

/// One published module and the surface admission compared it by.
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

impl PublicationTable {
    /// The native publications: every native actor namespace this binary
    /// links, read from the same link-time facts the address index reads.
    pub(super) fn from_inventory() -> Self {
        Self {
            namespaces: native_cardinality_facts()
                .map(|(namespace, _)| (Arc::from(namespace), Published::Native))
                .collect(),
        }
    }

    /// Who holds `namespace`, or `None` when it is unpublished.
    pub(super) fn holder(&self, namespace: &str) -> Option<Holder<'_>> {
        self.namespaces.get(namespace).map(|published| match published {
            Published::Native => Holder::Native,
            Published::Module(publication) => {
                Holder::Module { hash: publication.module.hash(), surface: &publication.surface }
            }
        })
    }

    /// Point every namespace `surface` exports at `module`. Called only by the
    /// registry owner's publish arm, after [`admit`] accepted the module.
    pub(super) fn publish(&mut self, module: Module, surface: ModuleSurface) {
        let publication = Arc::new(ModulePublication { module, surface });
        for namespace in publication.surface.exported_namespaces() {
            self.namespaces.insert(Arc::clone(namespace), Published::Module(Arc::clone(&publication)));
        }
    }
}
