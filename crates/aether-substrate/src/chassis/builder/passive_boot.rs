use crate::actor::native::ExportedHandles;
use crate::chassis::ctx::{ChassisCtx, FallbackRouter};
use crate::chassis::error::BootError;
use crate::config::{ConfigError, ConfigSources};

pub(super) trait DynShutdown {
    fn shutdown_dyn(self: Box<Self>);
}

/// Concrete adapter for the fallback-router slot. The handler itself
/// is owned by the chassis's `fallback` slot (claimed via
/// `ctx.claim_fallback_router`); this entry exists purely to keep
/// the boot-order / shutdown-order invariants aligned with cap
/// entries when `with_fallback_router` is mixed into a builder.
struct FallbackShutdown;

impl DynShutdown for FallbackShutdown {
    fn shutdown_dyn(self: Box<Self>) {
        // The fallback router doesn't own any threads or channels —
        // it's a single function pointer. Nothing to do here; the
        // chassis's `fallback` slot drops the `Arc` when the
        // `BootedPassives` drops.
    }
}

/// Issue 697: chassis boot is multi-pass. Every registered passive
/// walks `claim → init → wire → spawn` synchronized across all
/// passives — the chassis builder calls phase N on every passive
/// before any passive enters phase N+1. The boot ordering means:
///
/// - At `init` time, every peer mailbox is already claimed (claim
///   pass completed), so init's `Resolver::resolve_mailbox` reaches
///   every peer.
/// - At `wire` time, every actor has an `init`-built instance, so
///   wire-time mail to a peer queues in that peer's inbox; the
///   recipient's dispatcher hasn't started yet.
/// - The `spawn` pass starts dispatchers; queued wire mail processes
///   naturally as each comes up.
///
/// No drain barrier between spawn and steady state — issue 697 §"Why
/// no barrier" rejects waiting for inboxes to drain (breaks for
/// actors with async mail sources). Frame-bound actors that can't
/// tolerate a one-frame race against a peer's wire-emitted mail keep
/// load-bearing state in `init`, not `wire`.
///
/// Failure mode: any phase returning `Err` triggers
/// [`Self::cleanup_after_failure`] in reverse boot order on every
/// previously-advanced passive, then the error propagates. Already-
/// spawned dispatchers (only on a spawn-pass failure for a later
/// passive) shut down via the [`DynShutdown`] handles the spawn pass
/// produced.
pub(super) trait PassiveBoot: Send {
    /// Phase 0 (ADR-0156 §5) — resolve this passive's cap `Config` off the
    /// builder's source stack (programmatic > argv > env > file > default),
    /// ahead of Claim. Run in composition order over every passive before any
    /// enters Claim, so a resolution fault aborts boot before a single mailbox
    /// is reserved. Default no-op for non-actor passives (the fallback router)
    /// and for the claim-only `--describe` path, which never resolves a value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known env key, argv overlay value, or
    /// config-file section holds an unparseable value (ADR-0090 §4).
    fn resolve(&mut self, sources: &mut ConfigSources) -> Result<(), ConfigError> {
        let _ = sources;
        Ok(())
    }

    /// Phase 1 — claim namespace + mailbox; build per-cap transport
    /// + binding; stash claim resources for later phases.
    fn claim(&mut self, ctx: &mut ChassisCtx<'_>) -> Result<(), BootError>;

    /// Phase 2 — construct the actor instance via `A::init`. Default
    /// no-op for non-actor passives (e.g., the fallback router).
    fn init(&mut self, ctx: &mut ChassisCtx<'_>, handles: &mut ExportedHandles) -> Result<(), BootError> {
        let _ = ctx;
        let _ = handles;
        Ok(())
    }

    /// Phase 3 — post-init mail-allowed lifecycle hook
    /// ([`Lifecycle::wire`](aether_actor::Lifecycle::wire), ADR-0079 amended). Default no-op.
    fn wire(&mut self) -> Result<(), BootError> {
        Ok(())
    }

    /// Phase 4 — spawn dispatcher; produce a shutdown handle.
    /// Consumes the impl.
    fn spawn(self: Box<Self>, ctx: &mut ChassisCtx<'_>) -> Result<Box<dyn DynShutdown>, BootError>;

    /// Roll back any acquired resources after a phase returned `Err`
    /// on this impl, or after a sibling passive's later phase failed
    /// while this impl had already advanced. Idempotent across the
    /// pre-spawn phases. Consumes the impl.
    fn cleanup_after_failure(self: Box<Self>, ctx: &mut ChassisCtx<'_>);
}

/// Single-phase passive: the fallback router lives entirely in the
/// claim step (it stashes its handler into `ChassisCtx::fallback`).
/// `init` / `wire` are no-ops; `spawn` returns the no-op
/// [`FallbackShutdown`].
pub(super) struct FallbackRouterBoot {
    handler: Option<FallbackRouter>,
}

impl FallbackRouterBoot {
    pub(super) fn new(handler: FallbackRouter) -> Self {
        Self { handler: Some(handler) }
    }
}

impl PassiveBoot for FallbackRouterBoot {
    fn claim(&mut self, ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        let handler = self.handler.take().expect("FallbackRouterBoot::claim called twice");
        ctx.claim_fallback_router(handler)
    }

    fn spawn(self: Box<Self>, _ctx: &mut ChassisCtx<'_>) -> Result<Box<dyn DynShutdown>, BootError> {
        Ok(Box::new(FallbackShutdown))
    }

    fn cleanup_after_failure(self: Box<Self>, _ctx: &mut ChassisCtx<'_>) {
        // The router, once claimed, sits in `ctx.fallback` (an
        // `&mut Option<FallbackRouter>` borrowed from `BootedPassives`).
        // Boot failure unwinds the entire `BootedPassives`, so the
        // slot drops with it. Nothing to do here.
    }
}
