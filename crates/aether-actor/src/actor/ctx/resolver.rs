//! [`Resolver`] — init-time address resolution surface.
//!
//! Per-stage capability trait under the issue 663 refactor. Init ctxs
//! impl this; runtime and drop ctxs deliberately do not (resolution
//! belongs at boot — runtime resolution after init isn't a supported
//! shape).
//!
//! Today the FFI side is the only consumer: wasm guests resolve their
//! own mailbox id and subscribe to input streams via the trait.
//! Substrate's `NativeInitCtx` skips this trait — native caps use the
//! const paths (`mailbox_id_from_name`, `K::ID`) directly because they
//! sit on the same side of the FFI boundary as the registry.

use aether_data::Kind;

use crate::actor::ctx::mail_sender::MailSender;
use crate::mail::mailbox::{KindId, Mailbox};

/// Init-time resolution surface. The associated [`MailSender::Transport`]
/// pins the produced [`Mailbox`] to the same transport the ctx routes
/// through.
pub trait Resolver: MailSender {
    /// The component's own mailbox id — the value the substrate uses
    /// to address `receive` calls to this instance. Useful for
    /// hand-rolled subscribe / self-mailing at init time when the
    /// SDK's higher-level wrappers don't fit.
    fn mailbox_id(&self) -> u64;

    /// Resolve a kind by its `const ID`. Pure compile-time construction
    /// under ADR-0030 Phase 2 — no host-fn round trip, never fails.
    fn resolve<K: Kind>(&self) -> KindId<K>;

    /// Resolve a mailbox by name and bind it to kind `K`, producing a
    /// typed [`Mailbox<K, Self::Transport>`]. Pure compile-time
    /// construction.
    fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K, Self::Transport>;

    /// Send `aether.input.subscribe` with this component's mailbox as
    /// the subscriber for `K`. ADR-0068 keys subscriber sets by
    /// `KindId` directly, so this collapses to a one-line send: any
    /// `Kind` is sendable, the substrate's platform thread fans out
    /// only for kinds it actually publishes, and a subscribe for a
    /// non-stream kind is a harmless no-op.
    fn subscribe_input<K: Kind + 'static>(&self);
}
