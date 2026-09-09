//! The runtime every substrate chassis shares: the wasmtime engine, the mail
//! router, the kind manifest, and the reply-handle table. Chassis-specific
//! peripherals (window, GPU, TCP listener, event loop) live in the chassis
//! crate that depends on this one.
//!
//! The [`Chassis`] trait is universal but narrow: `const PROFILE` (the stable
//! identifier, `"desktop"` / `"headless"` / `"hub"` / `"substrate-harness"`),
//! `type Driver` (the capability that owns the main thread), `type Env` (the
//! resolved-config bag), and `fn build(env) -> Result<BuiltChassis<Self>,
//! BootError>`. What you `run()` is the [`BuiltChassis<Self>`] that `build`
//! returns, not a value of `Self` (ADR-0035, ADR-0071).
//!
//! Each loaded wasm component runs as an
//! `aether_component::trampoline::WasmTrampoline`, a native actor instanced
//! under `aether.embedded:NAME` that delegates incoming mail to the guest. A
//! trampoline trap fails fast at the trap site through
//! `NativeBinding::fatal_abort`; there is no per-frame drain barrier.

// The `#[actor] impl NativeActor for X` macro emits
// `impl ::aether_substrate::NativeDispatch for X` so external callers
// (the per-cap crates, user-crate caps) resolve
// unambiguously. For impls written *inside* aether-substrate the
// `::aether_substrate` prefix is in-crate; the self-alias makes
// absolute paths resolve without a separate "internal vs external"
// macro arm. (Pre-issue-654 the wasm trampoline was one such in-crate
// impl; post-654 it lives in `aether-component`, but the alias
// stays because future substrate-internal `#[actor]` impls would hit
// the same need.)
extern crate self as aether_substrate;

pub mod actor;
pub mod atomic_write;
// iamacoffeepot/aether#1275: `boot` builds a wasmtime `Engine` + `Linker`,
// so it rides the `wasm` feature. Default-on; only `aether-derive`'s
// trybuild fixtures opt out (they don't reach the boot path).
#[cfg(feature = "wasm")]
pub mod boot;
pub mod capture;
pub mod chassis;
pub mod config;
// ADR-0115 / ADR-0149: the domain-neutral content-addressed storage core
// the hub's `ArtifactStore` and Bloomery's `artifacts` port both consume.
// Beside `atomic_write` / `pid_lock`, the two primitives it builds on.
pub mod content_store;
pub mod mail;
pub mod net;
pub mod pid_lock;
#[cfg(feature = "render")]
pub mod render;
pub mod runtime;
pub mod scheduler;
// The one monotonic id counter the capabilities that mint caller-visible
// ids (render textures / geometries / programs, text fonts, audio banks)
// share, so they agree on what happens at the ceiling.
pub mod session_ids;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod transform;

pub use actor::monitor::MonitorHandle;
pub use actor::native::binding::NativeBinding;
pub use actor::native::ctx::{Erased, ExportedHandles, NativeCtx, NativeInitCtx};
// ADR-0112: the per-handler ctx reply-mode markers, re-exported next to
// `NativeCtx` so chassis / harness code naming `NativeCtx<'_, Manual>`
// reaches them without an `aether_actor` import.
pub use actor::native::envelope::Envelope;
pub use actor::native::spawn::{SpawnBuilder, SpawnError, Spawner, Subname};
// iamacoffeepot/aether#2311 (composed): the identity actor trait plus the
// native per-kind dispatch trait parameterised by the runtime state. The boot
// lifecycle is the shared `aether_actor::Lifecycle<S>` (no native re-export).
pub use actor::native::slot::pumped::PumpedSlot;
pub use actor::native::{Dispatch, NativeActor};
pub use actor::native::{HandlerSpawnBuilder, SpawnOutcome, SpawnReceipt};
pub use actor::registry::{ActorEntry, ActorRegistry, MonitorEntry, MonitorError};
#[cfg(feature = "wasm")]
pub use actor::wasm::component::{Component, ComponentCtx};
pub use aether_actor::{Addressable, root_mailbox};
pub use aether_actor::{Emit, Manual, Multi, ReplyMode, Single};
pub use aether_derive::{Config, StageArgv};
#[cfg(feature = "wasm")]
pub use boot::SubstrateBoot;
pub use chassis::builder::{
    Builder, BuilderState, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, HasDriver, NeverDriver,
    NeverDriverRunning, NoDriver, PassiveChassis, RunError,
};
pub use chassis::ctx::{
    ChassisCtx, DropOnShutdownClaim, FallbackRouter, MailboxClaim, MailboxSender, MailboxWakeSlot, SharedActorSlots,
};
pub use chassis::error::BootError;
pub use chassis::inbox::{InboundMail, SettlingInbox};
pub use chassis::{Chassis, engine_name};
pub use config::{
    ConfigError, ConfigManifest, ConfigMember, ConfigMemberRecord, ConfigProvenance, ConfigSources, FromArgvThenEnv,
    KnobKind, KnobRecord, KnownKeys, RingCapacities, SchedulerTuning, StageArgv, dump_config, file_section, known_keys,
    validate_env,
};
pub use mail::mailer::Mailer;
pub use mail::outbound::{DroppingBackend, EgressBackend, EgressEvent, HubOutbound, RecordingBackend};
pub use mail::registry::{
    ActorAddressInventoryError, AddressResolutionError, BootAuthority, InboxHandler, InlineHandler, MailboxEntry,
    OwnedDispatch, Registry, ResolvedAddress,
};
pub use mail::{KindId, Mail, MailKind, MailRef, MailboxId, RequestId, Source, SourceAddr};
pub use runtime::panic_hook::init_panic_hook;

/// Well-known mailbox name for substrate-level diagnostic events
/// delivered back to this engine. Today the only kind delivered here
/// is `aether.mail.unresolved` (issue #185), pushed by the hub when
/// an engine's bubbled-up mail (ADR-0037) can't be resolved at the
/// hub either. The sink handler re-warns via `tracing::warn!` so the
/// diagnostic surfaces in this engine's own `engine_logs`.
pub const AETHER_DIAGNOSTICS: &str = "aether.diagnostics";
