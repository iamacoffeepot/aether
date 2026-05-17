//! Issue 552 stage 1: native chassis-cap actor surface.
//!
//! The native counterpart of `aether_actor::FfiActor`. Stage 1
//! introduces the type-level vocabulary; Stage 2 migrated the
//! existing capabilities (Log, Handle, Io, Net, Audio, Render) onto
//! it. Stage 1's deliverable was the trait + ctx + dispatch
//! infrastructure plus a working boot path through
//! [`crate::chassis::builder::Builder::with_actor`]. The legacy
//! `with(cap)` / `Actor + Dispatch` facade path retired in issue 688
//! once every cap migrated to `with_actor`.
//!
//! ## Shape
//!
//! ```ignore
//! #[capability]
//! #[derive(Singleton)]
//! pub struct ExampleCap { /* plain fields — single-threaded ownership */ }
//!
//! #[actor]
//! impl NativeActor for ExampleCap {
//!     type Config = ();
//!     const NAMESPACE: &'static str = "aether.example";
//!
//!     fn init(_: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> { … }
//!
//!     #[handler] fn on_hello(&self, ctx: &mut NativeCtx<'_>, mail: Hello) { … }
//! }
//! ```
//!
//! Issue 629 / Phase A: actors are owned by their dispatcher thread
//! as `Box<A>` — the cross-thread `Arc<dyn Any + Send + Sync>` storage
//! is retired. [`NativeDispatch`] takes `&mut self`; `#[handler]`
//! methods can take either `&self` or `&mut self` (Phase B sweeps caps
//! to `&mut self` cap by cap as state migrates off interior mutability).
//!
//! Cross-thread access from drivers / embedders flows through
//! cap-exported sub-handles published in `init` via
//! [`ctx::NativeInitCtx::publish_handle`] and retrieved via
//! [`crate::DriverCtx::handle`]. The actor itself never escapes its
//! dispatcher thread.
//!
//! ## What does NOT live here
//!
//! - `actor::<A>()` lookups on per-handler ctx. Once dispatchers are
//!   running, caps and components communicate via mail — peering at
//!   sibling state recreates the shared-state coupling the actor
//!   model is designed to eliminate. The chassis-level
//!   `chassis.actor::<X>() -> Arc<X>` retired with issue 629 / Phase A;
//!   external runtimes (drivers, `TestBench`, MCP) reach for
//!   cap-exported handles instead.
//!
//! ## Catch-all caps (issue 576)
//!
//! Caps that fan-out every kind they're addressed at — broadcast
//! today, hub-as-actor in the future — author with a `#[fallback]`
//! method instead of `#[handler]`s. The macro emits a blanket
//! `impl<K: Kind> HandlesKind<K> for X {}` so typed sends like
//! `ctx.actor::<BroadcastCapability>().send(&payload)` compile for every K,
//! and overrides [`NativeDispatch::__aether_dispatch_fallback`] to
//! route every envelope through the user's fallback method. Hybrid
//! shape (typed handlers + fallback as a runtime safety net) is
//! rejected by the macro: strict receivers shouldn't silently swallow
//! unknown kinds.

pub mod binding;
pub mod ctx;
pub(crate) mod dispatch;
pub(crate) mod dispatcher_slot;
pub mod envelope;
pub mod mailbox;
pub mod spawn;
pub mod spawn_thread;

pub use binding::NativeBinding;
pub use ctx::{ExportedHandles, NativeCtx, NativeInitCtx};
pub use envelope::Envelope;
pub use mailbox::NativeActorMailbox;
pub use spawn::{SpawnBuilder, SpawnError, Spawner, Subname};
pub use spawn_thread::{InheritCtx, RootCtx};

use aether_actor::Actor;

use crate::chassis::error::BootError;
use crate::mail::KindId;

/// Native chassis-cap actor trait. Per-cap shape: one struct, one
/// `#[actor] impl NativeActor for X` block. The `Config` associated
/// type is moved into [`Self::init`] by the chassis builder; pass
/// `()` for caps with no configuration.
///
/// Issue 629 / Phase A: bound is `: Actor` only (which gives
/// `Send + 'static`). The dispatcher thread owns the cap as `Box<Self>`
/// for its lifetime — no cross-thread `Arc` share, no `Sync` bound.
/// Cap state can be plain fields without interior-mutability gymnastics
/// once Phase B sweeps each cap.
pub trait NativeActor: Actor {
    /// Configuration the chassis builder threads through to
    /// [`Self::init`]. `()` for caps without configuration; the
    /// actual config struct (e.g. `AudioConfig`) for caps that
    /// take one.
    type Config: Send + 'static;

    /// Boot the cap. The chassis has already claimed the cap's
    /// mailbox under `Actor::NAMESPACE` and built a fresh
    /// `NativeBinding` whose self-mailbox is that claim — the
    /// `ctx` exposes those (and the actors-so-far map for boot-time
    /// peer lookups) plus the universal handle-store for caps that
    /// hold typed handles.
    fn init(config: Self::Config, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError>
    where
        Self: Sized;

    /// Post-init mail-allowed hook (issue 584, ADR-0079 amended
    /// 2026-05-09). Runs after `init` returned `Ok` and the actor's
    /// mailbox is published, but before the dispatcher pulls the
    /// first envelope. The actor may send mail here — peers are
    /// addressable and the chassis is past the boot barrier. Default
    /// empty — opt-in for actors that need to register subscriptions,
    /// announce themselves, or kick off a poll loop via self-mail.
    fn wire(&mut self, _ctx: &mut NativeCtx<'_>) {}

    /// Pre-shutdown mail-allowed hook (issue 584, ADR-0079 amended
    /// 2026-05-09). Runs after the dispatcher's inbox drain, before
    /// the actor value drops. Triggers:
    ///
    /// - Self-shutdown — actor's handler called `ctx.shutdown()`;
    ///   dispatcher saw the flag set after the handler returned.
    /// - Substrate shutdown — chassis dropped its registry, the sink
    ///   handler's `Weak<Sender>` upgrade fails, the inbox channel
    ///   disconnects, and `recv_blocking` returns `None`.
    /// - Cooperative external — a peer mailed the actor a "please
    ///   close" kind; the actor's handler did its cleanup and called
    ///   `ctx.shutdown()`. From the dispatcher's perspective this is
    ///   identical to self-shutdown.
    ///
    /// Mail emitted from `unwire` lands in peer mailboxes if those
    /// peers are still alive; sends to a dead peer warn-drop. Use this
    /// to publish a final broadcast, flush state, or signal monitors.
    /// Default empty — opt-in.
    ///
    /// Issue 629 / Phase A: `&mut self` since the dispatcher thread
    /// owns the cap exclusively.
    fn unwire(&mut self, _ctx: &mut NativeCtx<'_>) {}
}

/// Sum dispatch entry-point — emitted once per `#[actor] impl
/// NativeActor for X` block. Takes the inbound mail's `(kind, payload)`
/// pair, routes by kind id to the right `#[handler]` method, and
/// returns `Some(())` on match + decode success or `None` on unknown
/// kind / decode failure.
///
/// Issue 629 / Phase A: `&mut self` since the dispatcher thread owns
/// the cap as `Box<Self>` and is the sole entry-point. `: Send +
/// 'static` (no `Sync`) — the cap is not shared across threads.
///
/// Per-handler-kind compile checks come from
/// [`aether_actor::HandlesKind`] (one impl per handler the macro
/// emits); a future per-K `NativeDispatch<K>` may layer on top if a
/// caller wants a typed `dispatch_kind::<K>` entry, but Phase A
/// doesn't need it.
pub trait NativeDispatch: Send + 'static {
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()>;

    /// Catch-all fallback for envelopes whose kind doesn't match any
    /// `#[handler]` (issue 576). Default returns `false` — the
    /// chassis trampoline warn-logs the unknown-kind miss as today.
    /// The `#[actor]` macro overrides this when the impl carries a
    /// `#[fallback]` method, returning `true` after the user's
    /// fallback runs so the trampoline knows to suppress the warn
    /// log.
    fn __aether_dispatch_fallback(
        &mut self,
        _ctx: &mut NativeCtx<'_>,
        _envelope: &Envelope,
    ) -> bool {
        false
    }
}
