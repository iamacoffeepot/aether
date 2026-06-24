//! The `aether.fs` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "runtime"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the `FsCapability`
//! identity never names these types nor pulls `aether_substrate`. The
//! substrate-typed imports are gated once by this module rather than
//! line-by-line; the `#[actor] impl` reaches the state, ctx, and fold helpers
//! through the single `use runtime::*` glob in the parent.

// Fs-level types the state and fold helpers name.
use super::{AdapterRegistry, FsFoldError, FsTransformError};

pub use std::any::Any;
pub use std::fs;
pub use std::panic::{self, AssertUnwindSafe};
pub use std::sync::Arc;

pub use super::adapter::fs_error_from_std;
pub use aether_data::TransformError;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::transform::{FoldError, TransformRegistry};

/// `aether.fs` runtime state (ADR-0041). Owns the resolved adapter
/// registry plus the link-time native-transform registry (ADR-0048 §2)
/// `on_fetch` uses to resolve and validate transform chains. The
/// dispatcher holds this as the cap's state and routes envelopes through
/// the macro-emitted `Dispatch` impl; replies return directly from the
/// `#[handler]` methods (ADR-0112). The addressing identity is the
/// distinct ZST `FsCapability`. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
pub struct FsCapabilityState {
    pub(super) registry: Arc<AdapterRegistry>,
    /// Link-time native-transform registry (ADR-0048 §2). Built once
    /// at `init`; immutable thereafter.
    pub(super) transforms: TransformRegistry,
}

pub fn map_fold_error(e: &FoldError) -> FsFoldError {
    match e {
        FoldError::UnknownTransform(id) => FsFoldError::UnknownTransform(*id),
        FoldError::NonLinearArity { at_index, arity } => FsFoldError::NonLinearArity {
            at_index: *at_index as u64,
            arity: *arity as u64,
        },
        FoldError::KindMismatch {
            at_index,
            expected,
            found,
        } => FsFoldError::KindMismatch {
            at_index: *at_index as u64,
            expected: *expected,
            found: *found,
        },
    }
}

pub fn map_transform_error(e: &TransformError) -> FsTransformError {
    match e {
        TransformError::InputDecode { slot } => {
            FsTransformError::InputDecode { slot: *slot as u64 }
        }
        TransformError::InputArity { expected, actual } => FsTransformError::InputArity {
            expected: *expected as u64,
            actual: *actual as u64,
        },
        TransformError::OutputOverflow { limit, actual } => FsTransformError::OutputOverflow {
            limit: *limit as u64,
            actual: *actual as u64,
        },
    }
}

pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

#[cfg(test)]
impl FsCapabilityState {
    /// Test-only direct constructor. Production boots through
    /// `Builder::with_actor::<FsCapability>(roots)` which calls the
    /// generated `Lifecycle::init`; handler-unit tests that want to drive
    /// a handler without a full chassis hand a pre-built registry directly.
    pub(crate) fn from_registry(registry: Arc<AdapterRegistry>) -> Self {
        Self {
            registry,
            transforms: TransformRegistry::from_inventory(),
        }
    }
}
