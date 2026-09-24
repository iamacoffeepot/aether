//! The boot-time context an actor's `init` receives.
//!
//! It dispatches nothing and mails nothing (issue 703: `init` is the
//! sync constructor, ADR-0079). What it carries is what construction needs —
//! the actor's own address, the chassis mailer, and the
//! [`ExportedHandles`] map a cap publishes a driver-facing sub-handle into.

use std::any::{Any, TypeId};
use std::sync::Arc;

use aether_data::{ActorMail, MailboxId};

use crate::actor::native::ActorProbe;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::self_wake::SelfWake;
use crate::mail::mailer::Mailer;

use super::ExportedHandles;

/// Boot-time context for [`Lifecycle::init`](aether_actor::Lifecycle::init). Carries a borrow of
/// the actor's transport (for init-time mail), a borrow of the
/// chassis's [`ExportedHandles`] map (so the cap can publish a
/// driver-facing sub-handle via [`Self::publish_handle`]), and a
/// clone of the substrate's mailer for caps that need to register an
/// outbound hook at boot.
///
/// Issue 629 / Phase A: the legacy `peer::<A>() -> Arc<A>` accessor
/// retired here (closes issue 628). Sibling caps communicate via mail
/// at runtime, from `wire` onward, through [`NativeCtx`](super::NativeCtx)'s send verbs; this ctx
/// has none. Caps that genuinely need cross-thread state
/// access from drivers / embedders publish a handle bundle via
/// [`Self::publish_handle`] and the consumer retrieves it through
/// [`crate::DriverCtx::handle`].
pub struct NativeInitCtx<'a> {
    binding: &'a Arc<NativeBinding>,
    handles: &'a mut ExportedHandles,
    mailer: Arc<Mailer>,
}

impl<'a> NativeInitCtx<'a> {
    /// Internal constructor — only [`crate::chassis::builder::Builder::with_actor`]
    /// builds these.
    pub(crate) fn new(binding: &'a Arc<NativeBinding>, handles: &'a mut ExportedHandles, mailer: Arc<Mailer>) -> Self {
        Self { binding, handles, mailer }
    }

    /// Borrow the Arc'd cap-bound [`NativeBinding`]. Used by the wasm
    /// trampoline at init to install itself on the
    /// [`crate::actor::wasm::component::ComponentCtx`] so the
    /// reply / outbound-mail host fns can route through this binding.
    /// Promoted from `pub(crate)` to `pub` by issue 654 when the
    /// trampoline moved to `aether-component` next to its consumer;
    /// no other external caller is intended.
    #[must_use]
    pub fn binding(&self) -> &Arc<NativeBinding> {
        self.binding
    }

    /// The actor's own [`MailboxId`] — the deterministic FNV-1a hash
    /// of its full registered name (ADR-0029). For singletons that's
    /// `Addressable::NAMESPACE`; for instanced actors it's
    /// `"{NAMESPACE}:{subname}"` (ADR-0079). No handler ctx has a mirror
    /// of it: the wasm trampoline's `init` is the one caller, and it goes
    /// with the trampoline's stored position in the closing slice of
    /// issue 6350.
    #[must_use]
    pub fn self_id(&self) -> MailboxId {
        self.binding.self_mailbox()
    }

    /// Clone the substrate's mailer. Caps that need to register a
    /// `Mailer::set_outbound`-style hook (Hub client, future
    /// fallback routers) reach for this; most caps don't need it.
    #[must_use]
    pub fn mailer(&self) -> Arc<Mailer> {
        Arc::clone(&self.mailer)
    }

    /// A [`SelfWake<K>`] for a thread this cap spawns during `init` — an
    /// accept loop, a socket reader, a timer — to wake this actor with one
    /// `K`. `init` itself still mails nothing: the handle only wakes once the
    /// thread calls it, and it names no position (ADR-0230).
    #[must_use]
    pub fn self_wake<K: ActorMail>(&self) -> SelfWake<K> {
        SelfWake::new(self.binding)
    }

    /// An [`ActorProbe`] for a thread this cap spawns during `init` that must
    /// decide about a peer it holds a proof of without a round trip through
    /// this actor. Like [`Self::self_wake`], it names no position and grants
    /// no send.
    #[must_use]
    pub fn actor_probe(&self) -> ActorProbe {
        self.mailer.actor_probe()
    }

    /// Issue 629 / Phase A: publish a sub-handle bundle for cross-
    /// thread access from drivers / embedders. The handle is stored in
    /// the chassis's [`ExportedHandles`] map keyed by `TypeId::of::<H>`
    /// and retrieved via [`crate::DriverCtx::handle`]. Caps that don't
    /// need driver-side state access never call this.
    ///
    /// `H: Any + Send + Sync` so the chassis-side map can hand the
    /// stored bundle back across thread boundaries; typically `H` is
    /// a small `Clone` handle struct (e.g. `HttpServerHandle`).
    pub fn publish_handle<H: Any + Send + Sync + 'static>(&mut self, handle: H) {
        self.handles.by_type.insert(TypeId::of::<H>(), Box::new(handle));
    }
}
// Issue 703: NativeInitCtx no longer impls `MailSender`.
// `init` is the sync constructor (ADR-0079) and must NOT mail —
// subscriptions, peer hellos, and self-mail kickoffs all belong in
// `wire`, where `NativeCtx` provides the full mail surface.
