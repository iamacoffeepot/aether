//! Shared pieces every cell uses: the child guard, the wire driver, the repo
//! builder, and the in-memory correspondence double.

pub mod client;
pub mod correspondence;
pub mod process;
pub mod repo;
pub mod wire;

pub use process::{Coordinator, Ingress, free_port};
