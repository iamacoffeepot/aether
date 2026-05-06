//! aether-substrate: runtime that every substrate chassis shares.
//!
//! Hosts the wasmtime engine, the mail router, the kind manifest, the
//! reply-handle table, and the hub-socket client. Chassis-specific
//! peripherals (window, GPU, TCP listener, event loop) live in the
//! chassis crate that binds this as a dependency. See ADR-0035.
//!
//! The wasm-component supervisor — the actor that owns the per-mailbox
//! component table, dispatcher threads, and the wasmtime Engine /
//! Linker — lives in `aether-capabilities` as
//! [`ControlPlaneCapability`][cp] (issue 603). Substrate exposes the
//! interface the supervisor implements via [`supervisor::ComponentRouter`]
//! plus the structured drain outcomes ([`supervisor::DrainSummary`])
//! the chassis frame loop matches on; it knows nothing about the
//! supervisor's identity beyond "something installed itself on
//! [`Mailer::install_component_router`]".
//!
//! The `Chassis` trait (ADR-0035, redefined by ADR-0071) is universal
//! but intentionally narrow: `const PROFILE` (the chassis's stable
//! identifier — `"desktop"`, `"headless"`, `"hub"`, `"test-bench"`),
//! `type Driver: DriverCapability` (the capability that owns the main
//! thread), `type Env` (resolved-config bag), and
//! `fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError>`.
//! The chassis instance you `run()` is the [`BuiltChassis<Self>`] the
//! trait method returns, not a value of `Self` itself.
//!
//! [cp]: https://docs.rs/aether-capabilities/latest/aether_capabilities/struct.ControlPlaneCapability.html

// Issue 552 stage 2: the `#[actor] impl NativeActor for X` macro
// emits `impl ::aether_substrate::NativeDispatch for X` so external
// callers (caps in user crates, `aether-capabilities` once the move
// in stage 2c lands) resolve unambiguously. For caps written *inside*
// aether-substrate (today: every cap under `capabilities/`) the
// `::aether_substrate` prefix is in-crate; the self-alias makes
// absolute paths resolve without a separate "internal vs external"
// macro arm.
extern crate self as aether_substrate;

pub mod boot;
pub mod capability;
pub mod capture;
pub mod chassis;
pub mod chassis_builder;
pub mod component;
pub mod control_helpers;
pub mod ctx;
pub mod frame_loop;
pub mod handle_store;
pub mod host_fns;
pub mod input;
pub mod kind_manifest;
pub mod lifecycle;
pub mod log_install;
pub mod mail;
pub mod mailer;
pub mod native_actor;
pub mod native_transport;
pub mod outbound;
pub mod panic_hook;
pub mod registry;
#[cfg(feature = "render")]
pub mod render;
pub mod reply_table;
pub mod supervisor;

pub use aether_actor::Actor;
pub use boot::{SubstrateBoot, SubstrateBootBuilder};
pub use capability::{
    ActorErased, BootError, BootedChassis, ChassisBuilder, ChassisCtx, DropOnShutdownClaim,
    Envelope, FallbackRouter, FrameBoundClaim, MailboxClaim, SinkSender, WedgedFrameBound,
};
pub use chassis::Chassis;
pub use chassis_builder::{
    Builder, BuilderState, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, HasDriver,
    NeverDriver, NeverDriverRunning, NoDriver, PassiveChassis, RunError,
};
pub use component::Component;
pub use ctx::SubstrateCtx;
pub use input::{InputSubscribers, new_subscribers, remove_from_all, subscribers_for};
pub use mail::{KindId, Mail, MailKind, MailboxId, ReplyTarget, ReplyTo};
pub use mailer::Mailer;
pub use native_actor::{Actors, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx};
pub use native_transport::NativeTransport;
pub use outbound::{
    DroppingBackend, EgressBackend, EgressEvent, HubOutbound, LogEntry, LogLevel, RecordingBackend,
};
pub use panic_hook::init_panic_hook;
pub use registry::{MailboxEntry, Registry, SinkHandler};
pub use supervisor::{ComponentRouter, ComponentSendOutcome, DrainDeath, DrainOutcome, DrainSummary};

/// Well-known mailbox name for substrate-level diagnostic events
/// delivered back to this engine. Today the only kind delivered here
/// is `aether.mail.unresolved` (issue #185), pushed by the hub when
/// an engine's bubbled-up mail (ADR-0037) can't be resolved at the
/// hub either. The sink handler re-warns via `tracing::warn!` so the
/// diagnostic surfaces in this engine's own `engine_logs`.
pub const AETHER_DIAGNOSTICS: &str = "aether.diagnostics";
