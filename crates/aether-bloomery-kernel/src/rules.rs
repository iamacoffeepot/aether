use aether_bloomery_kinds::{HeadMoved, OpaqueBytes, REACTORS_HEAD, ReactorSet};
use aether_bloomery_reactor::{Guard, reactor};
use aether_bloomery_view::Heads;

use crate::KernelReconcileIntent;

/// Declines a reactor-set move unless it addresses the conventional root.
struct MovedReactorSet;

impl Guard<HeadMoved<ReactorSet>> for MovedReactorSet {
    type Views = Heads;

    fn resolve(change: &HeadMoved<ReactorSet>, heads: &Heads) -> Option<Self> {
        (change.head() == &REACTORS_HEAD && heads.get(&REACTORS_HEAD) == Some(change.to())).then_some(Self)
    }
}

/// Admits byte-head moves after the fold has installed their new binding.
///
/// The kernel does not have the selected `ReactorSet` artifact in its `Heads`
/// view. The native adapter mechanically filters moves outside that set.
struct MovedBundleArtifact;

impl Guard<HeadMoved<OpaqueBytes>> for MovedBundleArtifact {
    type Views = Heads;

    fn resolve(change: &HeadMoved<OpaqueBytes>, heads: &Heads) -> Option<Self> {
        (heads.get(change.head()) == Some(change.to())).then_some(Self)
    }
}

/// Versioned WASM policy for configuration-triggered lifecycle requests.
pub struct KernelPolicy;

#[reactor]
impl Reactor for KernelPolicy {
    const NAMESPACE: &'static str = "bloomery.kernel.policy";

    #[rule]
    fn reconcile_set(
        &self,
        _change: HeadMoved<ReactorSet>,
        _root: MovedReactorSet,
        heads: Heads,
    ) -> KernelReconcileIntent {
        KernelReconcileIntent { event_seq: heads.cursor().0 }
    }

    #[rule]
    fn reconcile_bundle(
        &self,
        _change: HeadMoved<OpaqueBytes>,
        _bundle: MovedBundleArtifact,
        heads: Heads,
    ) -> KernelReconcileIntent {
        KernelReconcileIntent { event_seq: heads.cursor().0 }
    }
}
