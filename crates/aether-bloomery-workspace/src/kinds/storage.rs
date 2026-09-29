//! The wake the workspace's own workers send it when they have a storage
//! request for it to carry (ADR-0240 D7).

/// A worker queued a read or a stage: drain the queue and send each through
/// its task's source. Fieldless; the queue carries the requests.
#[aether_data::kind(name = "aether.workspace.storage_wake", copy, default, eq, no_serde)]
pub struct StorageWake;
