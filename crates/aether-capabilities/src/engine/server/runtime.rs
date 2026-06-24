//! The `aether.engine` engines-cap runtime half (ADR-0122 identity/runtime
//! split). Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a transport-only build of
//! the [`EngineServer`](super::EngineServer) identity never names these types
//! nor pulls `aether_substrate`. The substrate-typed imports are gated once by
//! this module rather than line-by-line; the `#[actor] impl` reaches the
//! state, ctx types, artifact/fleet helpers, and result kinds through the
//! single `use runtime::*` glob in the parent.

pub use crate::engine::kinds::ForwardEnvelope;
pub use crate::engine::proxy::{EngineProxy, EngineProxyConfig, HeartbeatParams};
pub use crate::engine::store::{ArtifactStore, LAYOUT_VERSION_DIR};
pub use aether_data::{EngineId, Kind, MailboxId, Uuid};
pub use aether_kinds::{
    DeadEngineDescriptor, DeathReason, EngineDescriptor, ListComponentBinariesResult,
    ListEngineBinariesResult, ListEnginesResult, ResolveComponentResult, SpawnEngineResult,
    TerminateEngineResult, UploadBinaryResult, UploadComponentResult,
};
pub use aether_substrate::Mail;
pub use aether_substrate::Subname;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::SourceAddr;
pub use aether_substrate::mail::mailer::Mailer;
pub use std::collections::HashMap;
pub use std::collections::VecDeque;
pub use std::path::PathBuf;
pub use std::process::{Command, Stdio};
pub use std::sync::Arc;
pub use std::time::{Duration, Instant};

// The artifact-store + fleet helpers the handlers delegate to live in the
// native-only `artifacts` / `fleet` submodules; re-export them here so the
// parent's `use runtime::*` glob reaches them alongside the rest of the
// runtime half.
pub use super::artifacts::{
    bootstrap_ingest, ingest_binary, ingest_component, realize_executable, resolve_component,
    resolve_selector,
};
pub use super::fleet::{engine_store_root, free_local_port, settle_err};

/// How many recently-died engines [`EngineServer`](super::EngineServer)
/// retains for `list_engines`' `recently_died` sidecar (issue 1906). A small
/// bound: the surface is "what just left and why", not an audit log —
/// the oldest record is dropped once the ring is full.
const RECENTLY_DIED_CAP: usize = 16;

/// One recently-departed engine in [`EngineServerState`]'s recently-died
/// ring (issue 1906). Cap-internal — holds the wire fields plus the
/// `Instant` the cap removed the engine, so `on_list` can compute the
/// `died_age_millis` it reports in a [`DeadEngineDescriptor`].
pub struct DeadRecord {
    pub(super) engine_id: String,
    pub(super) rpc_port: u16,
    pub(super) reason: DeathReason,
    pub(super) died_at: Instant,
}

/// One supervised engine in [`EngineServerState`]'s table.
pub struct EngineEntry {
    /// Mailbox of the `aether.engine.proxy:<id>` actor — the
    /// forward target for `TerminateEngine`.
    pub(super) proxy_mailbox: MailboxId,
    /// The localhost RPC port the cap assigned this substrate.
    pub(super) rpc_port: u16,
    /// When the cap last saw this engine alive (issue 1339): set at
    /// spawn (just-connected = alive) and refreshed on each
    /// `EngineAlive` the proxy reports from a confirmed `Pong`.
    /// `on_list` reports `now - last_alive` as the heartbeat age.
    pub(super) last_alive: Instant,
}

/// `aether.engine` runtime state (ADR-0122 split): supervises a fleet of
/// [`EngineProxy`] actors, one per spawned substrate. The addressing identity
/// is the distinct ZST [`EngineServer`](super::EngineServer); the dispatcher
/// holds this as the cap's state and routes envelopes through the
/// macro-emitted `Dispatch` impl. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without exposing
/// it as crate-public API.
pub struct EngineServerState {
    pub(super) engines: HashMap<EngineId, EngineEntry>,
    /// Monotonic source of `EngineId`s. Engine ids only need to be
    /// unique among the engines this cap currently supervises — a
    /// process-local counter delivers that without a `uuid` rng
    /// dependency. Starts at 1 (`Uuid::from_u128(0)` is the nil
    /// uuid).
    pub(super) next_engine_seq: u128,
    /// Cached so `on_route` can push a `ForwardEnvelope` at a proxy
    /// while *propagating the inbound reply-to* — `NativeCtx`'s
    /// sends stamp the cap as sender, but a routed call's reply
    /// must reach the originating `RpcServerCapability`, not here.
    pub(super) mailer: Arc<Mailer>,
    /// Liveness-heartbeat tuning each spawned proxy is armed with
    /// (issue 1339), resolved once from `EngineConfig` at init.
    /// `None` disables the heartbeat fleet-wide.
    pub(super) heartbeat: Option<HeartbeatParams>,
    /// Startup-dial connect budget each spawned proxy is armed with
    /// (issue 2072), resolved once from `EngineConfig` at init.
    /// `Some(d)` caps the retry; `None` is the wait-forever sentinel.
    pub(super) connect_budget: Option<Duration>,
    /// Bounded ring of the last [`RECENTLY_DIED_CAP`] engines that
    /// left the table and why (issue 1906). `on_terminate` /
    /// `on_engine_died` push a [`DeadRecord`] at the removal site;
    /// `on_list` renders it into the reply's `recently_died` sidecar
    /// so an observer can tell a clean terminate from a crash or a
    /// heartbeat eviction.
    pub(super) recently_died: VecDeque<DeadRecord>,
    /// Hub-scoped content-addressed binary store (ADR-0115, issue
    /// 1953) — the storage half of the artifact registry.
    /// `on_upload_binary` ingests a staged binary content-addressed;
    /// `on_list_engine_binaries` enumerates the stored entries. Built from
    /// `EngineConfig` (the layout dir + disk budget) at init so it
    /// persists across a `restart-hub` (the layout root outlives the
    /// hub child); the spawn cutover (#1954) reads it back through the
    /// store's `get` seam.
    pub(super) store: ArtifactStore,
}

impl EngineServerState {
    /// Push a [`DeadRecord`] onto the recently-died ring, evicting the
    /// oldest entry once the ring is full (issue 1906).
    pub(super) fn record_death(&mut self, engine_id: String, rpc_port: u16, reason: DeathReason) {
        if self.recently_died.len() >= RECENTLY_DIED_CAP {
            self.recently_died.pop_front();
        }
        self.recently_died.push_back(DeadRecord {
            engine_id,
            rpc_port,
            reason,
            died_at: Instant::now(),
        });
    }
}
