//! ADR-0075 §Decision 3 chassis-cap facade for the `aether.log`
//! mailbox. The cap and its [`LogBackend`] trait live here so wasm
//! senders can address the cap by type (`ctx.send::<LogCapability>`)
//! without pulling in any substrate-only types — the concrete backend
//! (with its `tracing` machinery, `cpal` device, wgpu state, etc.)
//! lives in `aether-substrate` and impls [`LogBackend`] there.
//!
//! The orphan rule lets `#[actor]` emit `HandlesKind<LogEvent> for
//! LogCapability<B>` here because the cap is local to this crate.
//! The substrate's concrete `LogTracingBackend` implements
//! [`LogBackend`]; runtime dispatch threads dispatch through the
//! cap's macro-emitted [`crate::Dispatch`] impl which delegates to
//! whichever backend is installed.

use crate::LogEvent;
use aether_data::Actor;

/// Substrate-side surface a chassis installs at boot. The chassis
/// reaches the runtime state (e.g. `tracing` subscriber, log facade
/// registration) through this trait — the cap struct itself just
/// holds the backend and forwards.
///
/// `Send + 'static` so the dispatcher thread can own the
/// [`LogCapability<B>`] (which owns `B`) for the cap's lifetime.
pub trait LogBackend: Send + 'static {
    /// Handle one decoded `LogEvent` envelope. Called from the
    /// chassis-side dispatcher thread for every `aether.log` mail
    /// the cap receives. ADR-0060 / ADR-0070 Phase 3 keep the
    /// substrate-side body as "decode level + emit through the
    /// `log` facade so `tracing`'s `tracing-log` integration lifts
    /// it back up." Delegated through this trait so the runtime
    /// state lives in `aether-substrate`.
    fn on_log_event(&mut self, event: LogEvent);
}

/// Default backend used for sender-side type resolution. Senders
/// write `LogCapability` (defaulting to [`ErasedLogBackend`]); the
/// chassis installs `LogCapability<LogTracingBackend>` at boot. Type
/// erased at the routing boundary — runtime dispatch reads
/// `LogCapability::<ErasedLogBackend>::NAMESPACE` (which doesn't
/// depend on `B`) to look up the registered instance.
///
/// All methods `unreachable!()` because no instance of this type is
/// ever installed at runtime — it exists purely for the compile-time
/// `Singleton + HandlesKind<LogEvent>` check on the sender side.
pub struct ErasedLogBackend;

impl LogBackend for ErasedLogBackend {
    fn on_log_event(&mut self, _event: LogEvent) {
        unreachable!("ErasedLogBackend used at runtime — chassis must install a real backend")
    }
}

/// `aether.log` mailbox cap. Wasm senders address it as
/// `ctx.send::<LogCapability>(&log_event)` — the type-level resolution
/// uses the default `ErasedLogBackend`, runtime dispatch routes by
/// `NAMESPACE` to whichever concrete `LogCapability<B>` the chassis
/// registered.
///
/// The substrate constructs `LogCapability::new(backend)` with a
/// concrete `B: LogBackend` (today: `LogTracingBackend`) and hands it
/// to the chassis builder. The chassis spawns a dispatcher thread
/// that owns the cap and routes inbound envelopes through the
/// macro-emitted [`aether_data::Dispatch`] impl.
pub struct LogCapability<B: LogBackend = ErasedLogBackend> {
    backend: B,
}

impl<B: LogBackend> LogCapability<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: LogBackend> Actor for LogCapability<B> {
    /// Components mail `aether.log` (kind id) to this mailbox; the
    /// SDK's `MailSubscriber` resolves through here. The
    /// `aether.<name>` form is the post-ADR-0074 Phase 5 convention
    /// for chassis-owned mailboxes.
    const NAMESPACE: &'static str = "aether.log";
}

impl<B: LogBackend> aether_data::Singleton for LogCapability<B> {}

/// `#[actor]` on the inherent impl emits:
///   - `impl<B: LogBackend> HandlesKind<LogEvent> for LogCapability<B>`
///   - `impl<B: LogBackend> Dispatch for LogCapability<B>` with a
///     decode-and-route body that calls `self.on_log_event(decoded)`.
///
/// The handler bodies are thin delegations to the backend trait — the
/// substantive logging work lives substrate-side.
#[aether_data::actor]
impl<B: LogBackend> LogCapability<B> {
    /// Forward a decoded log event to the backend.
    ///
    /// # Agent
    /// Components mail `aether.log` `LogEvent { level, target, message }`
    /// to this mailbox. The substrate's backend (`LogTracingBackend`)
    /// re-emits the event through the host's `tracing` pipeline so
    /// `engine_logs` (ADR-0023) sees it.
    #[aether_data::handler]
    fn on_log_event(&mut self, event: LogEvent) {
        self.backend.on_log_event(event);
    }
}
