//! Program declarations, in-process executors, and the driver that records one
//! execution as a transition or a fault.
//!
//! [`kinds::Program`] is the declaration as data. [`Program`] is its typed
//! mirror. [`declaration`] is the bridge: the same bytes every call, so the
//! same digest.

mod apply;
mod declare;
mod execute;
mod read;
mod registry;
mod staging;

pub use apply::{Applied, ApplyError, apply};
pub use declare::{Program, declaration, digest};
pub use execute::{Execute, Refusal};
pub use read::{ReadArtifacts, ReadError};
pub use registry::Executors;
pub use staging::{Execution, FinishError, Staging};

pub use aether_bloomery_kinds as kinds;
