use aether_bloomery_reactor::Output;

/// Request native reconciliation after the kernel observed a configuration move.
///
/// `event_seq` comes from the post-fold `Heads` cursor. The adapter correlates
/// this with the source cluster and its current activation attempt before
/// acting; this mail is never an independent activation record.
#[aether_data::kind(name = "bloomery.kernel.reconcile_intent", eq)]
pub struct KernelReconcileIntent {
    pub event_seq: u64,
}

impl Output for KernelReconcileIntent {}
