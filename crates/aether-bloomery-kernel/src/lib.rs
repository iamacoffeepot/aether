//! Journal-observing kernel reactor bundle.
//!
//! The generated views coordinator folds journal entries and delegates
//! matching configuration events to [`KernelPolicy`]. Rules emit only a typed
//! reconciliation request tied to the folded event sequence. A native adapter
//! later resolves the exact historical set and artifact heads, prepares the
//! affected component slots, and records lifecycle outcomes. These rules do
//! not load components, append events, or maintain an activation history.
//!
//! Load the generated coordinator at
//! [`aether_bloomery_reactor::CLUSTER_NAMESPACE`] with a
//! [`aether_bloomery_reactor::ClusterConfig`].

#![forbid(unsafe_code)]

mod intent;
mod rules;

pub use intent::KernelReconcileIntent;
pub use rules::KernelPolicy;

aether_actor::export!(KernelPolicy, generators = [aether_bloomery_reactor::bundle_reactors]);
