//! ADR-0075 chassis-cap facade for the `aether.handle` mailbox
//! (issue 533 PR D2). The cap and its [`HandleBackend`] trait live
//! here so wasm senders can address the cap by type
//! (`ctx.send::<HandleCapability>(&publish)`) without pulling in
//! substrate-only types — the concrete backend (with `HandleStore`
//! refcount machinery, `Arc<Mailer>` for replies) lives in
//! `aether-substrate` and impls [`HandleBackend`] there.
//!
//! Pre-PR-D2 the cap and dispatcher both lived in `aether-substrate`;
//! ADR-0075 splits them so chassis-vocab-importing callers don't pay
//! for `HandleStore` / `wgpu` / etc. transitive surface in their
//! dependency closure.

use crate::{HandlePin, HandlePublish, HandleRelease, HandleUnpin};
use aether_data::{Actor, ReplyTo};

/// Substrate-side surface a chassis installs at boot. Each method
/// takes the envelope's `sender: ReplyTo` (issue 533 PR D1) so the
/// backend can route the paired `*Result` reply through `Mailer::send_reply`.
///
/// `Send + 'static` so the dispatcher thread can own the
/// [`HandleCapability<B>`] (which owns `B`) for the cap's lifetime.
pub trait HandleBackend: Send + 'static {
    /// Publish bytes under a fresh handle id. Reply
    /// `HandlePublishResult::Ok { id, kind_id }` on success or
    /// `Err { error, kind_id }` on store failure.
    fn on_publish(&mut self, sender: ReplyTo, mail: HandlePublish);

    /// Decrement the refcount on a handle. Reply `HandleReleaseResult`
    /// echoing the id; `Err(UnknownHandle)` if the id doesn't resolve.
    fn on_release(&mut self, sender: ReplyTo, mail: HandleRelease);

    /// Mark a handle pinned (won't be evicted). Reply `HandlePinResult`.
    fn on_pin(&mut self, sender: ReplyTo, mail: HandlePin);

    /// Clear the pinned flag on a handle. Reply `HandleUnpinResult`.
    fn on_unpin(&mut self, sender: ReplyTo, mail: HandleUnpin);
}

/// Default backend used for sender-side type resolution. Senders
/// write `HandleCapability` (defaulting to [`ErasedHandleBackend`]);
/// the chassis installs `HandleCapability<HandleStoreBackend>` at
/// boot. All methods `unreachable!()` because no instance of this
/// type is ever installed at runtime — it exists purely for the
/// compile-time `Singleton + HandlesKind<K>` check on the sender side.
pub struct ErasedHandleBackend;

impl HandleBackend for ErasedHandleBackend {
    fn on_publish(&mut self, _sender: ReplyTo, _mail: HandlePublish) {
        unreachable!("ErasedHandleBackend used at runtime — chassis must install a real backend")
    }
    fn on_release(&mut self, _sender: ReplyTo, _mail: HandleRelease) {
        unreachable!("ErasedHandleBackend used at runtime — chassis must install a real backend")
    }
    fn on_pin(&mut self, _sender: ReplyTo, _mail: HandlePin) {
        unreachable!("ErasedHandleBackend used at runtime — chassis must install a real backend")
    }
    fn on_unpin(&mut self, _sender: ReplyTo, _mail: HandleUnpin) {
        unreachable!("ErasedHandleBackend used at runtime — chassis must install a real backend")
    }
}

/// `aether.handle` mailbox cap. Wasm senders address it as
/// `ctx.send::<HandleCapability>(&publish)` — the type-level
/// resolution uses the default `ErasedHandleBackend`, runtime
/// dispatch routes by `NAMESPACE` to whichever concrete
/// `HandleCapability<B>` the chassis registered.
pub struct HandleCapability<B: HandleBackend = ErasedHandleBackend> {
    backend: B,
}

impl<B: HandleBackend> HandleCapability<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: HandleBackend> Actor for HandleCapability<B> {
    /// ADR-0045 + ADR-0074 Phase 5: chassis-owned mailbox under the
    /// `aether.<name>` namespace.
    const NAMESPACE: &'static str = "aether.handle";
}

impl<B: HandleBackend> aether_data::Singleton for HandleCapability<B> {}

/// `#[actor]` on the inherent impl emits:
///   - `impl<B: HandleBackend> HandlesKind<HandlePublish> for HandleCapability<B>`
///     (and one per other handle kind).
///   - `impl<B: HandleBackend> Dispatch for HandleCapability<B>` with
///     a decode-and-route body that calls `self.on_<kind>(sender, decoded)`.
///
/// Each handler takes the 3-arg native shape `(&mut self, sender, mail)`
/// (issue 533 PR D1) so the backend can reply via `Mailer::send_reply`.
#[aether_data::actor]
impl<B: HandleBackend> HandleCapability<B> {
    /// Publish bytes under a fresh handle id.
    ///
    /// # Agent
    /// Reply: `HandlePublishResult`.
    #[aether_data::handler]
    fn on_publish(&mut self, sender: ReplyTo, mail: HandlePublish) {
        self.backend.on_publish(sender, mail);
    }

    /// Decrement a handle's refcount. SDK-side `Handle<K>::Drop`
    /// fires this; explicit `Ctx::release` paths also use it.
    ///
    /// # Agent
    /// Reply: `HandleReleaseResult`.
    #[aether_data::handler]
    fn on_release(&mut self, sender: ReplyTo, mail: HandleRelease) {
        self.backend.on_release(sender, mail);
    }

    /// Pin a handle so the LRU evictor skips it.
    ///
    /// # Agent
    /// Reply: `HandlePinResult`.
    #[aether_data::handler]
    fn on_pin(&mut self, sender: ReplyTo, mail: HandlePin) {
        self.backend.on_pin(sender, mail);
    }

    /// Clear the pinned flag on a handle.
    ///
    /// # Agent
    /// Reply: `HandleUnpinResult`.
    #[aether_data::handler]
    fn on_unpin(&mut self, sender: ReplyTo, mail: HandleUnpin) {
        self.backend.on_unpin(sender, mail);
    }
}
