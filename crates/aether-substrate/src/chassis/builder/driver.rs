use std::any::Any;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use crate::actor::native::ExportedHandles;
use crate::chassis::ctx::{ChassisCtx, FallbackRouter, MailboxClaim};
use crate::chassis::error::BootError;
use crate::mail::mailer::Mailer;

#[derive(Debug)]
pub enum RunError {
    Other(Box<dyn StdError + Send + Sync + 'static>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(e) => write!(f, "driver run failed: {e}"),
        }
    }
}

impl StdError for RunError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Other(e) => Some(&**e),
        }
    }
}

/// A driver capability owns the chassis main thread. Each chassis
/// composes exactly one driver alongside its passive capabilities.
/// The driver's [`DriverRunning::run`] body holds whatever loop the
/// chassis needs — winit on desktop, std-timer on headless, TCP
/// accept on hub.
///
/// Not `Send`: the desktop driver's `winit::EventLoop` is `!Send` on
/// macOS, so the driver and its running stay on the chassis main
/// thread end-to-end. The `Builder` holds the driver capability and
/// the resulting `Running` on a single-threaded code path between
/// [`Builder::driver`](crate::chassis::builder::Builder::driver) and
/// [`BuiltChassis::run`](crate::chassis::builder::BuiltChassis::run), so
/// neither needs to cross threads.
pub trait DriverCapability: 'static {
    type Running: DriverRunning;
    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError>;
}

/// Post-boot driver handle. Built once at chassis boot, then handed
/// to [`BuiltChassis::run`](crate::chassis::builder::BuiltChassis::run),
/// which calls [`DriverRunning::run`] on the calling thread. Returns
/// when the underlying loop drains
/// cleanly (window closed, accept loop done, shutdown signal).
pub trait DriverRunning: 'static {
    fn run(self: Box<Self>) -> Result<(), RunError>;
}

/// Phantom [`DriverCapability`] for passive chassis (substrate-harness, future
/// embedder-driven chassis kinds). The [`Chassis`](crate::chassis::Chassis)
/// trait requires `type Driver: DriverCapability`; passive chassis
/// declare this as their driver to satisfy the bound, but the value is
/// never instantiated (the `Builder<C, NoDriver>` path produces a
/// [`PassiveChassis<C>`](crate::chassis::builder::PassiveChassis) without
/// ever resolving `C::Driver`). Its `boot` is `unreachable!()` — reaching
/// it implies someone tried to drive a
/// chassis that has no driver, which is a programmer error rather than
/// a runtime condition.
pub struct NeverDriver;

impl DriverCapability for NeverDriver {
    type Running = NeverDriverRunning;
    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        unreachable!(
            "NeverDriver is a phantom for passive chassis; it should never be booted. \
             Build the chassis via its inherent `build_passive(env)` instead."
        );
    }
}

/// Running-side of [`NeverDriver`]; same unreachability contract.
pub struct NeverDriverRunning;

impl DriverRunning for NeverDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        unreachable!("NeverDriverRunning::run is never called by design");
    }
}

/// Boot-time context handed to a [`DriverCapability`]. Forwards the
/// passive [`ChassisCtx`] surface; pre-PR-E3 it also exposed typed
/// access to passive runnings via `expect` / `try_get`, but the
/// typed-runnings map retired alongside `Capability` so drivers
/// wanting cap state get it through pre-build accessors (today only
/// render via `RenderHandles`).
///
/// Issue 629 / Phase A: borrows the chassis's [`ExportedHandles`]
/// map. Drivers retrieve cap-published handle bundles via
/// [`Self::handle`]. The pre-629 `actor::<A>() -> Arc<A>` accessor
/// retired — the actor itself never escapes its dispatcher thread, so
/// drivers consume cap-exported handle clones instead.
pub struct DriverCtx<'a> {
    inner: ChassisCtx<'a>,
    handles: &'a ExportedHandles,
}

impl<'a> DriverCtx<'a> {
    pub(super) fn new(inner: ChassisCtx<'a>, handles: &'a ExportedHandles) -> Self {
        Self { inner, handles }
    }

    /// Drivers have no `NAMESPACE` const to delegate against — claim
    /// by explicit name.
    pub fn claim_mailbox(&mut self, name: &str) -> Result<MailboxClaim, BootError> {
        self.inner.claim_mailbox_with_override(name)
    }

    #[must_use]
    pub fn mail_send_handle(&self) -> Arc<Mailer> {
        self.inner.mail_send_handle()
    }

    pub fn claim_fallback_router(&mut self, handler: FallbackRouter) -> Result<(), BootError> {
        self.inner.claim_fallback_router(handler)
    }

    /// Issue 629 / Phase A: retrieve a clone of a cap-published handle
    /// bundle of type `H`. `None` if no cap published one (typically
    /// because the cap that owns the handle wasn't booted on this
    /// chassis). Drivers use this to pull `RenderHandles` and similar
    /// driver-facing sub-handle bundles without reaching for the cap
    /// itself.
    #[must_use]
    pub fn handle<H: Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.handles.get::<H>()
    }
}
