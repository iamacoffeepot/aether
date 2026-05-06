//! Issue #581 unified actor-aware logging — actor-bound only.
//!
//! Per-actor `tracing::*` events buffer into [`LogBuffer`] (a
//! [`Local`]); the chassis-pushed `ConfigureLogDrain` mail (auto-
//! handled by every `#[handlers]` derive, issue #601) installs
//! [`LogDrainSlot`] with the chassis-declared drain mailbox.
//! [`drain_buffer`] reads both slots and ships a [`LogBatch`] to that
//! mailbox at handler exit (and on `WARN`/`ERROR` priority flush
//! within the layer). The chassis dispatcher stamps the actor's own
//! transport-backed dispatch per handler via [`with_actor_dispatch`],
//! so the egress runs through the actor's own send path.
//!
//! There is no host branch. `tracing::*` events fired outside any
//! actor stamp (substrate boot, scheduler thread, panic hook) do NOT
//! enter the mail system — they hit stderr via the substrate's
//! registered fmt::Layer for operator visibility but never reach
//! `engine_logs`. The actor model's invariant is that every piece of
//! engine logic eventually runs as an actor; the host-branch shortcut
//! retired in issue #601 alongside `PROCESS` / `install_log_target` /
//! `ship_host_event`. Code that wants to surface in `engine_logs`
//! emits its events from inside an actor.
//!
//! Recursion guard: code inside [`drain_buffer`] routes through the
//! stamped transport's `send_mail`, which can emit its own
//! `tracing::*` events (e.g. the `capability mailbox sender dropped`
//! warn fired from a dead sink handler). Without a guard, those
//! events re-enter the layer, push the actor's buffer, priority-flush
//! at WARN, and recurse — observable as a stack overflow during
//! shutdown. The [`is_in_pipeline`] TLS flag short-circuits the
//! actor-aware path while a drain is in flight; events still flow to
//! the registered fmt::Layer so operators see them on stderr.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use aether_data::{Kind, KindId, MailboxId};
use aether_kinds::{LogBatch, LogEvent};
use tracing::{
    Event, Level,
    field::{Field, Visit},
};
// Wasm-only `tracing` imports needed by [`WasmSubscriber`]'s
// `Subscriber` impl. Kept under cfg so the host build doesn't warn
// about unused imports — `aether-substrate::log_install`'s
// `ActorAwareLayer` is the host-target hookup, not this subscriber.
#[cfg(target_arch = "wasm32")]
use tracing::{Subscriber, span};

use crate::{Local, local};

/// Per-actor log buffer, drained by the chassis dispatcher at
/// handler exit and on `WARN`/`ERROR` priority flush. Backed by
/// issue #582's [`Local`] primitive — one slot per actor, accessed
/// via TLS-routed [`crate::local::ActorSlots`].
#[derive(Default)]
pub struct LogBuffer(pub Vec<LogEvent>);

impl Local for LogBuffer {}

/// Per-actor log drain target — the `aether.log` (or chassis-
/// configured override) mailbox id [`drain_buffer`] ships
/// [`LogBatch`] mails to. Issue #601: replaces the retired
/// `PROCESS.log_mailbox` cell with a per-actor slot the chassis
/// installs via the `aether.control.configure_log_drain` mail
/// every actor's `#[handlers]` derive auto-handles.
///
/// Default `MailboxId(0)` — meaning "no drain configured." Until
/// the chassis-pushed `ConfigureLogDrain` mail arrives, drain calls
/// are no-ops, so init-body events buffer in [`LogBuffer`] without
/// being shipped (they flush at the first handler exit, which is
/// the `ConfigureLogDrain` handler installing this slot).
#[derive(Default)]
#[local]
pub struct LogDrainSlot(pub MailboxId);

/// Install `mailbox` as this actor's log drain target. Called from
/// the `#[handlers]`-derived `ConfigureLogDrain` handler the chassis
/// dispatches at instantiation; user code never names this directly.
///
/// Idempotent — overwriting is fine; the chassis sends one
/// `ConfigureLogDrain` per actor at boot, and a future per-actor
/// override extension would call this from the actor's own init.
pub fn set_drain(mailbox: MailboxId) {
    let _ = LogDrainSlot::try_with_mut(|s| s.0 = mailbox);
}

/// The currently-installed drain mailbox for the active actor, if
/// any. Returns `None` when no actor is stamped (host code, panic
/// hook) or when the slot is still at its default `MailboxId(0)`
/// (chassis hasn't dispatched `ConfigureLogDrain` yet, or chassis
/// declared no drain).
pub fn current_drain() -> Option<MailboxId> {
    let raw = LogDrainSlot::try_with(|s| s.0)?;
    if raw.0 == 0 { None } else { Some(raw) }
}

/// Mail egress hook the actor-aware drain path call into. The
/// chassis stamps an actor's transport per-handler via
/// [`with_actor_dispatch`]; [`drain_buffer`] reads the stamp to
/// route the actor's [`LogBatch`] through the same transport every
/// other outbound `send` uses. `aether-actor` doesn't name
/// `LogCapability` — the destination mailbox is per-actor-stamped via
/// [`LogDrainSlot`] (issue #601).
pub trait MailDispatch: Send + Sync {
    /// Push `payload` (already postcard-encoded `LogBatch` bytes) to
    /// `mailbox`. The implementer's `send_mail` attaches the actor's
    /// sender id automatically.
    fn send(&self, mailbox: MailboxId, kind: KindId, payload: &[u8]);
}

/// Every [`crate::transport::MailTransport`] is a valid
/// [`MailDispatch`] — `send_mail`'s signature already matches
/// what the drain path needs. Lets the chassis pass an actor's
/// transport into [`with_actor_dispatch`] without a hand-rolled
/// shim per call site.
impl<T> MailDispatch for T
where
    T: crate::transport::MailTransport + Send + Sync + ?Sized,
{
    fn send(&self, mailbox: MailboxId, kind: KindId, payload: &[u8]) {
        let _ = crate::transport::MailTransport::send_mail(self, mailbox.0, kind.0, payload, 1);
    }
}

/// Native-only per-handler dispatch stamp. Wasm runs single-
/// threaded inside one linear memory and uses
/// [`crate::WASM_TRANSPORT`] directly — the chassis-stamp /
/// TLS dance only matters on native where each actor owns its own
/// transport instance.
#[cfg(not(target_arch = "wasm32"))]
mod native_tls {
    extern crate std;
    use super::MailDispatch;
    use core::cell::Cell;

    std::thread_local! {
        pub(super) static ACTOR_DISPATCH: Cell<Option<&'static dyn MailDispatch>> =
            const { Cell::new(None) };
    }
}

/// RAII guard restoring the prior actor-dispatch stamp on drop.
#[cfg(not(target_arch = "wasm32"))]
struct DispatchGuard {
    prev: Option<&'static dyn MailDispatch>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for DispatchGuard {
    fn drop(&mut self) {
        native_tls::ACTOR_DISPATCH.with(|slot| slot.set(self.prev));
    }
}

/// Stamp `dispatch` as the current actor's mail-egress shim for the
/// duration of `f`. The chassis dispatcher trampoline calls this
/// around each handler dispatch (and around `init` if the actor's
/// init body emits log events). Restored on return / panic via the
/// drop guard.
///
/// Native-only: wasm doesn't carry a per-actor dispatch stamp —
/// `WASM_TRANSPORT` is a process global covering the single actor
/// in each linear memory.
///
/// SAFETY: the caller guarantees `dispatch` outlives `f`. Inside
/// the closure the stamped reference is treated as `'static`; the
/// guard restores the prior pointer before the surrounding stack
/// frame returns, so no `'static` reference escapes.
#[cfg(not(target_arch = "wasm32"))]
pub fn with_actor_dispatch<R>(dispatch: &dyn MailDispatch, f: impl FnOnce() -> R) -> R {
    let static_ref: &'static dyn MailDispatch =
        unsafe { core::mem::transmute::<&dyn MailDispatch, &'static dyn MailDispatch>(dispatch) };
    let _guard = native_tls::ACTOR_DISPATCH.with(|slot| {
        let prev = slot.get();
        slot.set(Some(static_ref));
        DispatchGuard { prev }
    });
    f()
}

/// Pop the current actor's [`LogBuffer`] and ship the contents as
/// one [`LogBatch`] mail to the configured target. No-op when the
/// buffer is empty, the [`LogDrainSlot`] is still at its default
/// (chassis hasn't dispatched `ConfigureLogDrain` yet, or chassis
/// declared no drain), or (on native) no [`with_actor_dispatch`] is
/// active.
pub fn drain_buffer() {
    let entries = match LogBuffer::try_with_mut(|b| core::mem::take(&mut b.0)) {
        Some(es) => es,
        None => return,
    };
    if entries.is_empty() {
        return;
    }
    // Issue #601: read the per-actor `LogDrainSlot` instead of the
    // retired `PROCESS.log_mailbox` cell. The chassis-pushed
    // `ConfigureLogDrain` mail (auto-handled by every `#[handlers]`
    // derive) installs the slot at the actor's first dispatched
    // handler.
    let Some(mailbox) = current_drain() else {
        return;
    };
    let batch = LogBatch { entries };

    let _guard = PipelineGuard::enter();

    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(dispatch) = native_tls::ACTOR_DISPATCH.with(|slot| slot.get()) else {
            return;
        };
        ship_batch(dispatch, mailbox, batch);
    }

    #[cfg(target_arch = "wasm32")]
    {
        ship_batch_via_wasm_transport(mailbox, batch);
    }
}

/// Re-entry guard for the log pipeline. While set, [`is_in_pipeline`]
/// returns `true` and the actor-aware tracing layer skips its
/// in-actor branch (events still reach the registered `tsfmt::Layer`
/// for stderr). The guard wraps the [`drain_buffer`] code path,
/// whose `MailDispatch::send` ↦ sink-handler chain can emit its own
/// `tracing::*` events (e.g. the `capability mailbox sender dropped`
/// warn fired from a dead sink handler). Without the guard, those
/// events re-enter the layer, push the actor's buffer, priority-
/// flush at WARN, and recurse — observable as a stack overflow
/// during shutdown.
#[cfg(not(target_arch = "wasm32"))]
mod pipeline_tls {
    extern crate std;
    use core::cell::Cell;

    std::thread_local! {
        pub(super) static IN_LOG_PIPELINE: Cell<bool> = const { Cell::new(false) };
    }
}

#[cfg(target_arch = "wasm32")]
mod pipeline_tls {
    use core::cell::Cell;

    pub(super) struct Slot(pub Cell<bool>);
    // SAFETY: wasm linear memory is single-threaded; the static is
    // reachable only from this actor's code.
    unsafe impl Sync for Slot {}
    pub(super) static IN_LOG_PIPELINE: Slot = Slot(Cell::new(false));

    impl Slot {
        pub fn with<R>(&'static self, f: impl FnOnce(&Cell<bool>) -> R) -> R {
            f(&self.0)
        }
    }
}

/// `true` iff we're currently inside the drain / host-ship path.
/// Read by [`crate::log::is_in_pipeline`] consumers (chiefly the
/// actor-aware layer in `aether-substrate::log_install`); set + cleared
/// by [`PipelineGuard`].
pub fn is_in_pipeline() -> bool {
    pipeline_tls::IN_LOG_PIPELINE.with(|cell| cell.get())
}

struct PipelineGuard;

impl PipelineGuard {
    fn enter() -> Self {
        pipeline_tls::IN_LOG_PIPELINE.with(|cell| cell.set(true));
        Self
    }
}

impl Drop for PipelineGuard {
    fn drop(&mut self) {
        pipeline_tls::IN_LOG_PIPELINE.with(|cell| cell.set(false));
    }
}

#[cfg(target_arch = "wasm32")]
fn ship_batch_via_wasm_transport(mailbox: MailboxId, batch: LogBatch) {
    use crate::transport::MailTransport;
    let bytes = match postcard::to_allocvec(&batch) {
        Ok(b) => b,
        Err(_) => return,
    };
    crate::WASM_TRANSPORT.send_mail(mailbox.0, <LogBatch as Kind>::ID.0, &bytes, 1);
}

fn ship_batch(dispatch: &dyn MailDispatch, mailbox: MailboxId, batch: LogBatch) {
    let bytes = match postcard::to_allocvec(&batch) {
        Ok(b) => b,
        Err(_) => return,
    };
    dispatch.send(mailbox, <LogBatch as Kind>::ID, &bytes);
}

/// Hard cap on the per-event message bytes. Trims oversize
/// payloads with a `" [truncated]"` suffix so a reader of
/// `engine_logs` can tell the source was longer.
const MAX_MESSAGE_BYTES: usize = 4096;
const TRUNCATED_SUFFIX: &str = " [truncated]";

pub fn encode_event(event: &Event<'_>) -> LogEvent {
    let metadata = event.metadata();
    let level = level_to_u8(*metadata.level());
    let target = metadata.target().to_string();

    let mut visitor = MessageBuilder::new();
    event.record(&mut visitor);
    let message = visitor.finish();

    LogEvent {
        level,
        target,
        message,
    }
}

pub(crate) fn level_to_u8(level: Level) -> u8 {
    match level {
        Level::TRACE => 0,
        Level::DEBUG => 1,
        Level::INFO => 2,
        Level::WARN => 3,
        Level::ERROR => 4,
    }
}

/// Walks an `Event`'s fields and renders them in fields-first order:
/// `key1=val1 key2=val2 message_body`. Matches `tracing-subscriber`'s
/// default fmt layer so a reader of `engine_logs` sees the same
/// shape regardless of which side emitted the event.
struct MessageBuilder {
    fields: String,
    message: String,
}

impl MessageBuilder {
    fn new() -> Self {
        Self {
            fields: String::new(),
            message: String::new(),
        }
    }

    fn finish(mut self) -> String {
        if !self.fields.is_empty() && !self.message.is_empty() {
            self.fields.push(' ');
        }
        self.fields.push_str(&self.message);
        truncate(self.fields)
    }

    fn append_field(&mut self, name: &str, separator: &str, value: core::fmt::Arguments<'_>) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(&mut self.fields, "{}{}{}", name, separator, value);
    }
}

impl Visit for MessageBuilder {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(&mut self.message, "{:?}", value);
        } else {
            self.append_field(field.name(), "=", format_args!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.append_field(field.name(), "=", format_args!("{}", value));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.append_field(field.name(), "=", format_args!("{}", value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.append_field(field.name(), "=", format_args!("{}", value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.append_field(field.name(), "=", format_args!("{}", value));
    }
}

fn truncate(mut s: String) -> String {
    if s.len() <= MAX_MESSAGE_BYTES {
        return s;
    }
    let mut cap = MAX_MESSAGE_BYTES.saturating_sub(TRUNCATED_SUFFIX.len());
    while cap > 0 && !s.is_char_boundary(cap) {
        cap -= 1;
    }
    s.truncate(cap);
    s.push_str(TRUNCATED_SUFFIX);
    s
}

/// Wasm linear memory's tracing global default — every `tracing::*`
/// event in component code lands here. The component runs as one
/// actor, so [`LogBuffer::try_with_mut`] always succeeds; we never
/// reach the host branch on this target.
///
/// Native chassis composes `ActorAwareLayer` (in `aether-capabilities`)
/// with `EnvFilter` + `tsfmt::Layer` via `tracing-subscriber`; that
/// crate is `std`-only so we can't pull it into `aether-actor`'s
/// `no_std` build. The wasm path runs without a filter or stderr
/// formatter — every event ships back to the chassis where its own
/// subscriber stack handles it.
#[cfg(target_arch = "wasm32")]
pub struct WasmSubscriber {
    next_span: core::sync::atomic::AtomicU64,
}

#[cfg(target_arch = "wasm32")]
impl WasmSubscriber {
    pub const fn new() -> Self {
        Self {
            next_span: core::sync::atomic::AtomicU64::new(1),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Default for WasmSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_arch = "wasm32")]
impl Subscriber for WasmSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        // Filtering happens on the substrate side; the wasm
        // subscriber forwards everything so the host's `EnvFilter`
        // sees the guest's reported target.
        true
    }

    fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
        let id = self
            .next_span
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        span::Id::from_u64(id.max(1))
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        // Re-entry guard: we're inside `drain_buffer` /
        // `ship_host_event`. Drop the event to keep the pipeline
        // from looping.
        if is_in_pipeline() {
            return;
        }
        let entry = encode_event(event);
        let level = entry.level;
        // Push to the actor's buffer. `try_with_mut` is `Some` on
        // wasm (linear memory IS the actor); we ignore the `None`
        // arm because it can't fire on this target.
        let _ = LogBuffer::try_with_mut(|b| b.0.push(entry));
        // Priority flush on warn/error so trap-time data survives.
        if level >= 3 {
            drain_buffer();
        }
    }
}

#[cfg(target_arch = "wasm32")]
static WASM_INSTALLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Install the wasm-side actor-aware subscriber as `tracing`'s
/// global default. Called from the `export!` macro before the
/// guest's `Component::init` runs (so logging from `init` works).
/// Idempotent.
#[cfg(target_arch = "wasm32")]
pub fn install_wasm_subscriber() {
    use core::sync::atomic::Ordering;
    if WASM_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = tracing::dispatcher::set_global_default(tracing::dispatcher::Dispatch::new(
        WasmSubscriber::new(),
    ));
}

/// `aether::log_trace!("msg")` — equivalent to `tracing::trace!`.
#[macro_export]
macro_rules! log_trace {
    ($($t:tt)*) => { ::tracing::trace!($($t)*) };
}

/// `aether::log_debug!("msg")` — equivalent to `tracing::debug!`.
#[macro_export]
macro_rules! log_debug {
    ($($t:tt)*) => { ::tracing::debug!($($t)*) };
}

/// `aether::log_info!("msg")` — equivalent to `tracing::info!`.
#[macro_export]
macro_rules! log_info {
    ($($t:tt)*) => { ::tracing::info!($($t)*) };
}

/// `aether::log_warn!("msg")` — equivalent to `tracing::warn!`.
#[macro_export]
macro_rules! log_warn {
    ($($t:tt)*) => { ::tracing::warn!($($t)*) };
}

/// `aether::log_error!("msg")` — equivalent to `tracing::error!`.
#[macro_export]
macro_rules! log_error {
    ($($t:tt)*) => { ::tracing::error!($($t)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_mapping() {
        assert_eq!(level_to_u8(Level::TRACE), 0);
        assert_eq!(level_to_u8(Level::DEBUG), 1);
        assert_eq!(level_to_u8(Level::INFO), 2);
        assert_eq!(level_to_u8(Level::WARN), 3);
        assert_eq!(level_to_u8(Level::ERROR), 4);
    }

    #[test]
    fn truncate_preserves_short_messages() {
        let s = String::from("short message");
        let out = truncate(s);
        assert_eq!(out, "short message");
    }

    #[test]
    fn truncate_appends_suffix_when_oversize() {
        let s = "a".repeat(MAX_MESSAGE_BYTES + 100);
        let out = truncate(s);
        assert!(out.ends_with(TRUNCATED_SUFFIX));
        assert!(out.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let mut s = String::with_capacity(MAX_MESSAGE_BYTES + 4);
        for _ in 0..(MAX_MESSAGE_BYTES / 4 + 5) {
            s.push('🦀');
        }
        let out = truncate(s);
        assert!(out.ends_with(TRUNCATED_SUFFIX));
    }
}
