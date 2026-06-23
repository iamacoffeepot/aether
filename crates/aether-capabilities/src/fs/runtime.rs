//! `aether.fs` runtime — the state-bearing half of the cap (spike:
//! identity/runtime split). Compiled only under `feature = "runtime"`.
//! Everything that names an `aether_substrate` type lives here, behind
//! the one feature gate and cfg-free within, so a transport-only build of
//! [`FsCapability`](super::FsCapability) (its identity + addressing
//! markers) never pulls the substrate runtime.

use std::sync::Arc;

use aether_substrate::transform::TransformRegistry;

use super::AdapterRegistry;

/// `aether.fs` runtime state (ADR-0041). Owns the resolved adapter
/// registry plus the link-time native-transform registry (ADR-0048 §2)
/// `on_fetch` uses to resolve and validate transform chains. The
/// dispatcher holds this as `Box<FsState>` and routes envelopes through
/// the macro-emitted `NativeDispatch` impl; replies are returned directly
/// from the `#[handler]` methods (ADR-0112). The addressing identity is
/// `FsCapability`, distinct from this struct (the spike's whole point).
pub struct FsState {
    pub(super) registry: Arc<AdapterRegistry>,
    /// Link-time native-transform registry (ADR-0048 §2). Built once at
    /// `init`; immutable thereafter.
    pub(super) transforms: TransformRegistry,
}

#[cfg(test)]
impl FsState {
    /// Test-only direct constructor. Production boots through
    /// `Builder::with_actor::<FsCapability>(roots)` which calls the
    /// generated `Lifecycle::init`; handler-unit tests that want to drive
    /// a handler without a full chassis hand a pre-built registry directly.
    pub(super) fn from_registry(registry: Arc<AdapterRegistry>) -> Self {
        Self {
            registry,
            transforms: TransformRegistry::from_inventory(),
        }
    }
}
