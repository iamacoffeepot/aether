//! aether-substrate: runtime that every substrate chassis shares.
//!
//! Hosts the wasmtime engine, the mail scheduler, the per-mailbox
//! component table, the kind manifest, the reply-handle table, the
//! control-plane dispatcher, and the hub-socket client. Chassis-specific
//! peripherals (window, GPU, TCP listener, event loop) live in the
//! chassis crate that binds this as a dependency. See ADR-0035.
//!
//! The `Chassis` trait (ADR-0035, redefined by ADR-0071) is universal
//! but intentionally narrow: `const PROFILE` (the chassis's stable
//! identifier — `"desktop"`, `"headless"`, `"hub"`, `"test-bench"`),
//! `type Driver: DriverCapability` (the capability that owns the main
//! thread), `type Env` (resolved-config bag), and
//! `fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError>`.
//! The chassis instance you `run()` is the [`BuiltChassis<Self>`] the
//! trait method returns, not a value of `Self` itself. Chassis-specific
//! control kinds (desktop's `capture_frame` / `set_window_mode` /
//! `platform_info`, hub's future routing/operator kinds) ride through
//! `ControlPlane::chassis_handler` — the fallback closure core's
//! dispatch falls into for unknown kinds. That keeps any single
//! chassis from having to implement `Unsupported` stubs for
//! operations it doesn't support.
//!
//! Helpers for chassis-side handlers live under `control`:
//! `decode_payload` and `resolve_bundle` are pub so chassis dispatch
//! can validate mail bundles the same way core does.

pub mod boot;
pub mod capabilities;
pub mod capability;
pub mod capture;
pub mod chassis;
pub mod chassis_builder;
pub mod component;
pub mod control;
pub mod ctx;
pub mod frame_loop;
pub mod handle_sink;
pub mod handle_store;
pub mod host_fns;
pub mod input;
pub mod kind_manifest;
pub mod lifecycle;
pub mod log_capture;
pub mod log_sink;
pub mod mail;
pub mod mailer;
pub mod outbound;
pub mod panic_hook;
pub mod registry;
#[cfg(feature = "render")]
pub mod render;
pub mod reply_table;
pub mod scheduler;

pub use boot::{ChassisHandlerContext, SubstrateBoot, SubstrateBootBuilder};
pub use capability::{
    BootError, BootedChassis, Capability, ChassisBuilder, ChassisCtx, Envelope, FallbackRouter,
    MailboxClaim, RunningCapability,
};
pub use chassis::Chassis;
pub use chassis_builder::{
    Builder, BuilderState, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, HasDriver,
    NeverDriver, NeverDriverRunning, NoDriver, PassiveChassis, RunError,
};
pub use component::Component;
pub use control::{AETHER_CONTROL, ChassisControlHandler, ControlPlane};
pub use ctx::SubstrateCtx;
pub use input::{InputSubscribers, new_subscribers, remove_from_all, subscribers_for};
pub use mail::{KindId, Mail, MailKind, MailboxId, ReplyTarget, ReplyTo};
pub use mailer::Mailer;
pub use outbound::{
    DroppingBackend, EgressBackend, EgressEvent, HubOutbound, LogEntry, LogLevel, RecordingBackend,
};
pub use panic_hook::init_panic_hook;
pub use registry::{MailboxEntry, Registry, SinkHandler};
pub use scheduler::Scheduler;

/// Well-known mailbox name for fan-out to every attached Claude
/// session (ADR-0008). A component or substrate-owned sink sends to
/// this name the same way it sends to any local sink; the forwarder
/// translates to `EngineToHub::Mail { address: Broadcast, ... }`.
pub const HUB_CLAUDE_BROADCAST: &str = "hub.claude.broadcast";

/// Well-known mailbox name for substrate-level diagnostic events
/// delivered back to this engine. Today the only kind delivered here
/// is `aether.mail.unresolved` (issue #185), pushed by the hub when
/// an engine's bubbled-up mail (ADR-0037) can't be resolved at the
/// hub either. The sink handler re-warns via `tracing::warn!` so the
/// diagnostic surfaces in this engine's own `engine_logs`.
pub const AETHER_DIAGNOSTICS: &str = "aether.diagnostics";
