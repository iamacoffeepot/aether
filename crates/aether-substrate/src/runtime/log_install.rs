//! Issue #601 substrate-side install for the actor-aware logging
//! path. The host branch retired alongside `PROCESS` /
//! `install_log_target` / `ship_host_event` from `aether-actor::log`:
//! `tracing::*` events emitted outside any actor stamp (substrate
//! boot, scheduler thread, panic hook) hit stderr via the registered
//! fmt::Layer for operator visibility but do not enter the mail
//! system. Until those code paths run as actors, their events stay
//! out of `engine_logs`. The chassis-pushed `ConfigureLogDrain` mail
//! and per-actor [`aether_actor::log::LogDrainSlot`] handle every
//! actor-bound case.
//!
//! Two entry points:
//!   - [`init_subscriber`] — called from `SubstrateBoot::build`.
//!     Installs `EnvFilter` + `tsfmt::Layer` + [`ActorAwareLayer`]
//!     as `tracing`'s global default, and registers
//!     [`ship_via_stamped_dispatch`] as `aether-actor::log`'s native
//!     log shipper. Idempotent.
//!   - [`with_actor_dispatch`] — called from each chassis dispatcher
//!     trampoline. Stamps an actor's transport into TLS for the
//!     duration of a handler so the actor's `tracing::*` events drain
//!     through that transport's `send_mail`.

use std::cell::Cell;

use aether_actor::Local;
use aether_actor::log::{LogBuffer, NativeLogShipper, drain_buffer, encode_event};
use aether_data::{KindId, MailboxId};

use crate::actor::native::binding::NativeBinding;
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt as tsfmt};

/// Mail egress hook the actor-aware drain path calls into. Lives in
/// the substrate side because chassis machinery is the only consumer
/// — the wasm side ships through `FFI_TRANSPORT` directly. The
/// chassis stamps an actor's transport per-handler via
/// [`with_actor_dispatch`]; the substrate-registered
/// `aether-actor` shipper hook ([`ship_via_stamped_dispatch`])
/// reads the stamp to route the actor's `LogBatch` through the same
/// transport every other outbound `send` uses.
pub trait MailDispatch: Send + Sync {
    /// Push `payload` (already postcard-encoded `LogBatch` bytes) to
    /// `mailbox`. The implementer's `send_mail` attaches the actor's
    /// sender id automatically.
    fn send(&self, mailbox: MailboxId, kind: KindId, payload: &[u8]);
}

/// Direct [`MailDispatch`] impl for [`NativeBinding`]. Issue 665
/// retired the cross-target `MailTransport` trait that previously
/// gated this as a blanket impl; today the only `MailDispatch`
/// consumer is the chassis-stamped per-actor logging path, and
/// `NativeBinding` is the only type that reaches it (FFI guests
/// drain log batches through `FFI_TRANSPORT`'s bridge, not through
/// `MailDispatch`).
impl MailDispatch for NativeBinding {
    fn send(&self, mailbox: MailboxId, kind: KindId, payload: &[u8]) {
        let _ = NativeBinding::send_mail(self, mailbox.0, kind.0, payload, 1);
    }
}

std::thread_local! {
    static ACTOR_DISPATCH: Cell<Option<&'static dyn MailDispatch>> =
        const { Cell::new(None) };
}

/// RAII guard restoring the prior actor-dispatch stamp on drop.
struct DispatchGuard {
    prev: Option<&'static dyn MailDispatch>,
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        ACTOR_DISPATCH.with(|slot| slot.set(self.prev));
    }
}

/// Stamp `dispatch` as the current actor's mail-egress shim for the
/// duration of `f`. The chassis dispatcher trampoline calls this
/// around each handler dispatch (and around `init` if the actor's
/// init body emits log events). Restored on return / panic via the
/// drop guard.
///
/// Native-only: wasm doesn't carry a per-actor dispatch stamp —
/// `FFI_TRANSPORT` is a process global covering the single actor in
/// each linear memory.
///
/// SAFETY: the caller guarantees `dispatch` outlives `f`. Inside the
/// closure the stamped reference is treated as `'static`; the guard
/// restores the prior pointer before the surrounding stack frame
/// returns, so no `'static` reference escapes.
pub fn with_actor_dispatch<R>(dispatch: &dyn MailDispatch, f: impl FnOnce() -> R) -> R {
    // SAFETY: same justification as the `SAFETY` paragraph in the
    // fn doc — the caller guarantees `dispatch` outlives `f`. The
    // `'static` reborrow is confined to the surrounding stack frame
    // by the `DispatchGuard` drop, so no `'static` reference escapes
    // past `with_actor_dispatch`'s return.
    let static_ref: &'static dyn MailDispatch =
        unsafe { core::mem::transmute::<&dyn MailDispatch, &'static dyn MailDispatch>(dispatch) };
    let _guard = ACTOR_DISPATCH.with(|slot| {
        let prev = slot.get();
        slot.set(Some(static_ref));
        DispatchGuard { prev }
    });
    f()
}

/// Native log shipper registered with `aether-actor::log` at boot.
/// Reads the TLS-stamped [`MailDispatch`] and ships `payload` through
/// it. No-op when no dispatch is stamped (out-of-actor `drain_buffer`
/// calls — shouldn't happen in normal flow).
fn ship_via_stamped_dispatch(mailbox: MailboxId, kind: KindId, payload: &[u8]) {
    let Some(dispatch) = ACTOR_DISPATCH.with(|slot| slot.get()) else {
        return;
    };
    dispatch.send(mailbox, kind, payload);
}

/// Tracing layer that routes in-actor events into the per-actor
/// [`LogBuffer`] for the chassis-installed drain to ship as
/// [`aether_kinds::LogBatch`] mail. Out-of-actor events drop here —
/// stderr fmt::Layer, registered alongside in [`init_subscriber`],
/// keeps them visible to operators. Issue #601 retired the
/// host-branch shortcut that previously routed out-of-actor events
/// through a process-global egress; the actor model's invariant is
/// that engine logic eventually runs as an actor, and code that
/// hasn't been migrated yet stays out of `engine_logs`.
pub struct ActorAwareLayer;

impl<S> Layer<S> for ActorAwareLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Re-entry guard: events emitted from inside `drain_buffer`
        // (e.g. the `capability mailbox sender dropped` warn fired
        // during shutdown) would otherwise loop the pipeline. Stderr
        // fmt still receives the event via the registered fmt::Layer.
        if aether_actor::log::is_in_pipeline() {
            return;
        }
        let entry = encode_event(event);
        let level = entry.level;
        // `try_with_mut` returns `Some` only when the chassis
        // dispatcher has stamped an actor's slots (in-actor branch).
        // Out-of-actor events drop here and leave `engine_logs`
        // unchanged.
        if LogBuffer::try_with_mut(|b| b.0.push(entry)).is_some() && level >= 3 {
            drain_buffer();
        }
    }
}

const FILTER_ENV: &str = "AETHER_LOG_FILTER";

/// Install the tracing subscriber stack: `EnvFilter` (reads
/// `AETHER_LOG_FILTER`, default `info`) + `tsfmt::Layer` to stderr +
/// [`ActorAwareLayer`]. Also registers [`ship_via_stamped_dispatch`]
/// as `aether-actor::log`'s native shipper so `drain_buffer` calls
/// from native actors route through the chassis-stamped transport.
/// Called from `SubstrateBoot::build`; idempotent (later calls no-op
/// via `try_init`; the shipper hook overwrite is a no-op since it's
/// the same pointer).
pub fn init_subscriber() {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tsfmt::layer().with_writer(std::io::stderr))
        .with(ActorAwareLayer)
        .try_init();
    aether_actor::log::set_native_log_shipper(ship_via_stamped_dispatch as NativeLogShipper);
}
