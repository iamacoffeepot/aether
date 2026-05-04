//! ADR-0075 chassis-cap facade for the `aether.io` mailbox (issue
//! 533 PR D3). The cap and its [`IoBackend`] trait live here so
//! wasm senders can address the cap by type
//! (`ctx.send::<IoCapability>(&read)`) without pulling in the
//! substrate-only adapter machinery — the concrete backend (with
//! `AdapterRegistry`, `LocalFileAdapter`, namespace roots) lives in
//! `aether-substrate` and impls [`IoBackend`] there.

use crate::{Delete, List, Read, Write};
use aether_data::{Actor, ReplyTo};

/// Substrate-side surface a chassis installs at boot. All four
/// methods are reply-bearing — the backend uses `sender` to route
/// the paired `*Result` through `Mailer::send_reply`.
///
/// `Send + 'static` so the dispatcher thread can own the cap.
pub trait IoBackend: Send + 'static {
    /// Read bytes from `(namespace, path)`. Reply `ReadResult`
    /// echoing the namespace + path on both arms.
    fn on_read(&mut self, sender: ReplyTo, mail: Read);

    /// Write bytes to `(namespace, path)`. Reply `WriteResult`
    /// echoing namespace + path; the bytes are dropped from the
    /// echo to keep mb-scale writes from producing mb-scale replies.
    fn on_write(&mut self, sender: ReplyTo, mail: Write);

    /// Delete a path under `namespace`. Reply `DeleteResult`.
    fn on_delete(&mut self, sender: ReplyTo, mail: Delete);

    /// List entries under `(namespace, prefix)`. Reply `ListResult`.
    fn on_list(&mut self, sender: ReplyTo, mail: List);
}

/// Default backend used for sender-side type resolution. Senders
/// write `IoCapability` (defaulting to [`ErasedIoBackend`]); the
/// chassis installs `IoCapability<AdapterIoBackend>` at boot.
pub struct ErasedIoBackend;

impl IoBackend for ErasedIoBackend {
    fn on_read(&mut self, _sender: ReplyTo, _mail: Read) {
        unreachable!("ErasedIoBackend used at runtime — chassis must install a real backend")
    }
    fn on_write(&mut self, _sender: ReplyTo, _mail: Write) {
        unreachable!("ErasedIoBackend used at runtime — chassis must install a real backend")
    }
    fn on_delete(&mut self, _sender: ReplyTo, _mail: Delete) {
        unreachable!("ErasedIoBackend used at runtime — chassis must install a real backend")
    }
    fn on_list(&mut self, _sender: ReplyTo, _mail: List) {
        unreachable!("ErasedIoBackend used at runtime — chassis must install a real backend")
    }
}

/// `aether.io` mailbox cap.
pub struct IoCapability<B: IoBackend = ErasedIoBackend> {
    backend: B,
}

impl<B: IoBackend> IoCapability<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: IoBackend> Actor for IoCapability<B> {
    /// ADR-0041 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.io";
}

impl<B: IoBackend> aether_data::Singleton for IoCapability<B> {}

#[aether_data::actor]
impl<B: IoBackend> IoCapability<B> {
    /// Read bytes from a logical namespace path.
    ///
    /// # Agent
    /// Reply: `ReadResult`. Echoes namespace + path on both arms.
    #[aether_data::handler]
    fn on_read(&mut self, sender: ReplyTo, mail: Read) {
        self.backend.on_read(sender, mail);
    }

    /// Write bytes to a logical namespace path. Atomic via tmp+rename
    /// in the local file adapter; semantics may differ in future
    /// adapters (cloud, in-memory).
    ///
    /// # Agent
    /// Reply: `WriteResult`. Echoes namespace + path (NOT bytes).
    #[aether_data::handler]
    fn on_write(&mut self, sender: ReplyTo, mail: Write) {
        self.backend.on_write(sender, mail);
    }

    /// Delete a path under a namespace.
    ///
    /// # Agent
    /// Reply: `DeleteResult`. Echoes namespace + path.
    #[aether_data::handler]
    fn on_delete(&mut self, sender: ReplyTo, mail: Delete) {
        self.backend.on_delete(sender, mail);
    }

    /// List entries under a namespace prefix.
    ///
    /// # Agent
    /// Reply: `ListResult`. Echoes namespace + prefix.
    #[aether_data::handler]
    fn on_list(&mut self, sender: ReplyTo, mail: List) {
        self.backend.on_list(sender, mail);
    }
}
