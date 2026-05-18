//! [`Resolver`] — init-time address resolution surface.
//!
//! Per-stage capability trait under the issue 663 refactor. Init ctxs
//! impl this; runtime and drop ctxs deliberately do not (resolution
//! belongs at boot — runtime resolution after init isn't a supported
//! shape).
//!
//! Issue 703 narrowed the trait to pure addressing (no [`MailSender`]
//! supertrait, no subscribe surface). The init stage genuinely needs
//! to look up its own mailbox id and resolve kind / mailbox tokens,
//! but it must NOT send mail — ADR-0079's `init` is the sync
//! constructor and mailing belongs in `wire`. Stripping the supertrait
//! makes that boundary structural rather than convention.
//!
//! Subscribing to input streams is just a regular mail send to
//! `aether.input` (the `InputCapability` cap, in `aether-capabilities`),
//! not a special trait method — the receiver-side handler decoded the
//! [`SubscribeInput`] payload and inserted into its subscriber table.
//! Components write
//! `ctx.send::<InputCapability, _>(&SubscribeInput { kind, mailbox })`
//! from `wire` directly.
//!
//! [`MailSender`]: crate::actor::ctx::MailSender
//! [`SubscribeInput`]: aether_kinds::SubscribeInput

use aether_data::Kind;

use crate::mail::mailbox::{KindId, Mailbox};

/// Init-time resolution surface. Pure addressing — no mail sends.
/// Components subscribe to input streams from `wire` (where the ctx
/// impls both `Resolver` and [`crate::actor::ctx::MailSender`]) by
/// sending [`SubscribeInput`](aether_kinds::SubscribeInput) directly
/// to the `InputCapability` (in `aether-capabilities`).
pub trait Resolver {
    /// The component's own mailbox id — the value the substrate uses
    /// to address `receive` calls to this instance.
    fn mailbox_id(&self) -> u64;

    /// Resolve a kind by its `const ID`. Pure compile-time construction
    /// under ADR-0030 Phase 2 — no host-fn round trip, never fails.
    fn resolve<K: Kind>(&self) -> KindId<K>;

    /// Resolve a mailbox by name and bind it to kind `K`, producing a
    /// typed [`Mailbox<K>`]. Pure compile-time construction; the
    /// returned token is pure addressing — sends route through each
    /// ctx's inherent / trait-provided `send` methods, not through
    /// the mailbox itself.
    fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K>;
}
