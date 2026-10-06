//! The spike's mail: the one job kind every producer handles, the gatherer's
//! control mail, and the batch a subscriber receives.

use aether_data::ErasedActorPath;
use serde::{Deserialize, Serialize};

/// Log `lines` lines.
pub const OP_WORK: u32 = 0;
/// Mail this actor `count` `OP_WORK` jobs under the running chain.
pub const OP_KICK: u32 = 1;
/// Log `lines` lines, then mail this actor the same job on a fresh chain
/// until the stop flag is set.
pub const OP_LOOP: u32 = 2;

#[repr(C)]
#[aether_data::kind(name = "spike.log.job", pod, default, eq)]
pub struct Job {
    pub op: u32,
    pub count: u32,
    pub lines: u32,
    /// Non-zero pushes straight into the ring the way the host does for a
    /// guest log call; zero goes through `tracing`.
    pub direct: u32,
}

/// Leave the tap closed and only publish the gatherer's handles.
pub const CONTROL_ATTACH: u32 = 0;
/// Open the tap and subscribe the gatherer to `Tick`.
pub const CONTROL_OPEN: u32 = 1;

#[repr(C)]
#[aether_data::kind(name = "spike.log.control", pod, default, eq)]
pub struct Control {
    pub op: u32,
    pub mode: u32,
    pub backfill: u32,
    pub buffer_cap: u32,
    pub pending_cap: u32,
    pub batch_cap: u32,
    pub shards: u32,
    pub reserved: u32,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct LogLine {
    pub actor: ErasedActorPath,
    pub level: u8,
    pub target: String,
    pub message: String,
    pub timestamp_unix_millis: u64,
}

#[aether_data::kind(name = "spike.log.lines")]
pub struct LogLines {
    pub elapsed_micros: u64,
    pub first_sequence: u64,
    pub skipped: u64,
    pub lines: Vec<LogLine>,
}
