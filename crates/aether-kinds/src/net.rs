//! ADR-0075 chassis-cap facade for the `aether.net` mailbox (issue
//! 533 PR D3). The cap and its [`NetBackend`] trait live here so
//! wasm senders can address the cap by type
//! (`ctx.send::<NetCapability>(&fetch)`) without pulling in
//! substrate-only types — the concrete backend (HTTP adapter, env-
//! resolved config) lives in `aether-substrate` and impls
//! [`NetBackend`] there.

use crate::Fetch;
use aether_data::{Actor, ReplyTo};

/// Substrate-side surface a chassis installs at boot. Reply-bearing
/// like Handle and Io: the backend uses `sender` to route the paired
/// `FetchResult` through `Mailer::send_reply`.
///
/// `Send + 'static` so the dispatcher thread can own the cap (which
/// owns `B`) for the cap's lifetime.
pub trait NetBackend: Send + 'static {
    /// Run a fetch request synchronously on the dispatcher thread
    /// and reply with a [`crate::FetchResult`]. ADR-0043 §2 flags
    /// in-thread sync fetches as the head-of-line blocking source
    /// to fix via a multi-threaded dispatcher; the trait shape
    /// stays unchanged when that lands.
    fn on_fetch(&mut self, sender: ReplyTo, mail: Fetch);
}

/// Default backend used for sender-side type resolution. Senders
/// write `NetCapability` (defaulting to [`ErasedNetBackend`]); the
/// chassis installs `NetCapability<UreqNetBackend>` (or the disabled
/// stub when the chassis declines net access). `unreachable!()`
/// because no instance of this type is ever installed at runtime.
pub struct ErasedNetBackend;

impl NetBackend for ErasedNetBackend {
    fn on_fetch(&mut self, _sender: ReplyTo, _mail: Fetch) {
        unreachable!("ErasedNetBackend used at runtime — chassis must install a real backend")
    }
}

/// `aether.net` mailbox cap.
pub struct NetCapability<B: NetBackend = ErasedNetBackend> {
    backend: B,
}

impl<B: NetBackend> NetCapability<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: NetBackend> Actor for NetCapability<B> {
    /// ADR-0043 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.net";
}

impl<B: NetBackend> aether_data::Singleton for NetCapability<B> {}

#[aether_data::actor]
impl<B: NetBackend> NetCapability<B> {
    /// Run a fetch request and reply with the response.
    ///
    /// # Agent
    /// Reply: `FetchResult`. Synchronous on the dispatcher thread —
    /// long-running fetches block other net mail until they finish.
    #[aether_data::handler]
    fn on_fetch(&mut self, sender: ReplyTo, mail: Fetch) {
        self.backend.on_fetch(sender, mail);
    }
}
