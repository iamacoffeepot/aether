//! Shared pieces every cell uses: the child guard, the wire driver, the repo
//! builder, the in-memory correspondence double, and the eager-integration
//! journal reads.

pub mod client;
pub mod correspondence;
pub mod eager;
pub mod process;
pub mod repo;
pub mod wire;

pub use process::{Coordinator, Ingress, free_port};
