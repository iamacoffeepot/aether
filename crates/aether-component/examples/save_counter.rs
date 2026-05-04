//! Save-counter example for ADR-0042. On first tick the component
//! synchronously reads a `u64` counter from `save://counter.bin`,
//! increments it, writes it back, and broadcasts the new value as
//! `demo.save_counter.count` on `hub.claude.broadcast`. Every
//! subsequent boot of the component sees the incremented count —
//! proof that persistent storage + the drain+buffer sync-I/O flow
//! compose end-to-end.
//!
//! The whole thing fits in a straight-line `on_tick` body because
//! `io::read_sync` / `io::write_sync` hide the mpsc drain loop
//! behind a linear call. The async equivalent would need a phase
//! enum + two `#[handler]` methods to cover the read → write state
//! machine.
//!
//! Run via MCP:
//!
//! 1. `spawn_substrate` a desktop / headless chassis with this
//!    component preloaded.
//! 2. `receive_mail` — `demo.save_counter.count` frames surface
//!    with the current counter value.
//! 3. `terminate_substrate`, spawn another, observe the count
//!    bumped by one.

use aether_component::{Component, Ctx, InitCtx, Mailbox, handlers, io, resolve_mailbox};
use aether_kinds::{IoError, Tick};

/// Broadcast payload the Claude session (or any component listening
/// on `hub.claude.broadcast`) reads to track counter progress. The
/// kind's schema rides in this wasm's `aether.kinds` custom section,
/// so the hub registers it automatically at load.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "demo.save_counter.count")]
pub struct Count {
    pub count: u64,
}

/// Namespace + path under which the counter is persisted.
const SAVE_NAMESPACE: &str = "save";
const SAVE_PATH: &str = "counter.bin";
/// Timeout for each sync I/O call. Generous — the local file adapter
/// should complete in sub-ms; larger backends (future cloud adapter)
/// would want a bigger budget.
const IO_TIMEOUT_MS: u32 = 1_000;
/// Broadcast sink — `hub.claude.broadcast` fans out to every
/// attached Claude session. `Count` is postcard-shaped; the unified
/// `Mailbox::send` routes through `Kind::encode_into_bytes`, which the
/// derive specializes to postcard here (issue #240).
const BROADCAST: Mailbox<Count> = resolve_mailbox::<Count>("hub.claude.broadcast");

pub struct SaveCounter {
    initialized: bool,
}

/// Reads a counter, increments it, writes it back — sync. On first
/// tick only; subsequent ticks are no-ops.
///
/// # Agent
/// `spawn_substrate` with this component and poll `receive_mail`;
/// each fresh instance bumps the counter by one.
#[handlers]
impl Component for SaveCounter {
    fn init(_ctx: &mut InitCtx<'_>) -> Self {
        SaveCounter { initialized: false }
    }

    /// First tick drives the sync read → increment → write cycle.
    ///
    /// # Agent
    /// Not typically sent manually; the substrate's tick loop fires
    /// this. Watch `receive_mail` for a `demo.save_counter.count`
    /// frame after the component loads.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        if self.initialized {
            return;
        }
        self.initialized = true;

        let current = read_counter_or_zero();
        let next = current.saturating_add(1);
        let _ = io::write_sync(
            SAVE_NAMESPACE,
            SAVE_PATH,
            &next.to_le_bytes(),
            IO_TIMEOUT_MS,
        );
        BROADCAST.send(ctx.transport(), &Count { count: next });
    }
}

aether_component::export!(SaveCounter);

/// Read the counter from disk. On first run the file doesn't exist;
/// we treat `NotFound` as "start at zero" rather than an error. Any
/// other I/O failure (corrupt bytes, forbidden namespace) falls
/// back to zero too — the demo prefers forward progress over
/// loudness; a real persistence layer would surface the error.
fn read_counter_or_zero() -> u64 {
    match io::read_sync(SAVE_NAMESPACE, SAVE_PATH, IO_TIMEOUT_MS) {
        Ok(bytes) if bytes.len() == 8 => {
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&bytes);
            u64::from_le_bytes(arr)
        }
        Ok(_) => 0,
        Err(io::SyncIoError::Io(IoError::NotFound)) => 0,
        Err(_) => 0,
    }
}
