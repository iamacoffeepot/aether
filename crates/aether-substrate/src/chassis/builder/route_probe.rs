//! A read-only probe of the route table's hot read path, for the registry
//! benchmark (iamacoffeepot/aether#6693).
//!
//! The benchmark measures the published view a running engine dispatches
//! against, so it needs the real `Registry`'s reads under load. This probe is
//! those reads and nothing else: it answers by proven reference, mutates
//! nothing, sends nothing, and hands out no position.

use std::sync::Arc;

use aether_actor::ErasedActorRef;
use aether_data::KindId;

use crate::mail::registry::{Registry, RegistryQueueMetrics, RouteResolution};

/// A read-only probe of the route table's hot read path, for the registry
/// benchmark. Answers by proven reference; mutates nothing, sends nothing.
/// Minted by [`PassiveChassis::route_read_probe`](super::PassiveChassis::route_read_probe).
///
/// Consumer: `aether-harness-substrate`'s `perf::registry` (the
/// `aether-perf-registry` binary).
#[derive(Clone)]
pub struct RouteReadProbe {
    registry: Arc<Registry>,
}

impl RouteReadProbe {
    pub(crate) const fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    /// The state of the route `kind` would take to `target`, read off the
    /// published view alone — the read a dispatch performs.
    #[must_use]
    pub fn resolve_route_state(&self, kind: KindId, target: ErasedActorRef) -> RouteResolution {
        self.registry.resolve_route_state(kind, target.id())
    }

    /// Whether a kind with id `kind` is registered.
    #[must_use]
    pub fn kind_registered(&self, kind: KindId) -> bool {
        self.registry.kind_name_shared(kind).is_some()
    }

    /// The ADR-0165 registry owner's queue metrics, or `None` before the owner
    /// is installed.
    #[must_use]
    pub fn owner_queue_metrics(&self) -> Option<RegistryQueueMetrics> {
        self.registry.owner_queue_metrics()
    }
}
