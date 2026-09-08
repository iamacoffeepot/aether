//! The chassis-owned map of cap-exported sub-handles.
//!
//! The one thing an actor may hand across its dispatcher thread's boundary.
//! Caps publish into it during `init`; drivers and embedders read a clone
//! back out through [`crate::DriverCtx::handle`].

use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Issue 629 / Phase A: type-keyed map of cap-exported sub-handles
/// for cross-thread access from drivers / embedders. Caps publish
/// during `init` via [`NativeInitCtx::publish_handle`](super::NativeInitCtx::publish_handle); consumers
/// retrieve via [`crate::DriverCtx::handle`]. Owned by
/// `BootedPassives`; borrowed mutably into each cap's [`NativeInitCtx`](super::NativeInitCtx)
/// in turn, then borrowed immutably by `DriverCtx`.
///
/// Replaces the pre-629 `Actors` struct that stored `Arc<dyn Any +
/// Send + Sync>` per booted cap — the cross-thread `Arc<A>` share was
/// the worker-pool-era legacy ADR-0038 made obsolete. Handles are
/// keyed by *handle* `TypeId` (e.g. `HttpServerHandle`), not by *actor*
/// `TypeId`, since the actor itself never escapes its dispatcher
/// thread.
#[derive(Default)]
pub struct ExportedHandles {
    pub(crate) by_type: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl ExportedHandles {
    #[must_use]
    pub fn new() -> Self {
        Self { by_type: HashMap::new() }
    }

    /// Retrieve a cloned copy of the published handle bundle of type
    /// `H`, or `None` if no cap published one. The chassis-side
    /// reader; caps publish via [`NativeInitCtx::publish_handle`](super::NativeInitCtx::publish_handle).
    #[must_use]
    pub fn get<H: Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.by_type.get(&TypeId::of::<H>()).and_then(|b| b.downcast_ref::<H>()).cloned()
    }

    /// `true` when no cap has published a handle yet. Useful for tests.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }

    /// Number of published handle bundles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_type.len()
    }
}
