// Wire-encode: `usize → u32` narrowings forward batch lengths to the
// wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! [`FfiActorMailbox`] — actor-typed sender handle for FFI guests.
//!
//! Issue 665 split the prior parametric `ActorMailbox<'a, R, T>` into
//! per-side types so the `MailTransport` trait can retire. The FFI
//! variant is lifetime-free — it carries no transport reference
//! because the FFI imports are global to the loaded module
//! ([`MAIL_BRIDGE`] is the dispatch surface).
//!
//! Built via [`crate::ffi::ctx::FfiCtx::actor`] /
//! [`crate::ffi::ctx::FfiCtx::resolve_actor`] and their init/drop
//! variants. The compile-time `R: HandlesKind<K>` gate is the same as
//! the prior parametric form: `ctx.actor::<RenderCapability>().send(&triangle)`
//! compiles only when `RenderCapability: HandlesKind<DrawTriangle>`.

use core::marker::PhantomData;

use aether_data::{ActorId, Kind, Tag, fold_lineage, mailbox_id_from_name, with_tag};

use crate::actor::{Actor, HandlesKind};
use crate::ffi::bridge::MAIL_BRIDGE;

/// Phantom-typed receiver-actor handle for FFI guests. ZST modulo the
/// stored mailbox id; cheap to construct, cheap to copy, no borrow
/// bookkeeping (the global [`MAIL_BRIDGE`] static covers dispatch).
pub struct FfiActorMailbox<R> {
    mailbox: u64,
    _r: PhantomData<fn() -> R>,
}

impl<R> Copy for FfiActorMailbox<R> {}
impl<R> Clone for FfiActorMailbox<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> FfiActorMailbox<R> {
    /// Not part of the public API; the ctx-level constructors go
    /// through here so the field stays private.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64) -> Self {
        Self {
            mailbox,
            _r: PhantomData,
        }
    }

    /// The receiver's typed mailbox id. Exposed for callers that need
    /// it for diagnostics or a host fn the SDK doesn't yet wrap.
    #[must_use]
    pub fn mailbox_id(&self) -> aether_data::MailboxId {
        aether_data::MailboxId(self.mailbox)
    }

    /// Resolve a sibling mailbox on the same transport, addressed by
    /// `name`. Same FNV-hash name resolution as
    /// [`crate::ffi::FfiCtx::resolve_actor`] — kept as an inherent
    /// method so cap-owned ext traits (which only have a mailbox in
    /// hand, not a ctx) can hand back peer handles without rethreading
    /// the ctx.
    #[must_use]
    pub fn resolve_peer<Peer: Actor>(&self, name: &str) -> FfiActorMailbox<Peer> {
        FfiActorMailbox::__new(mailbox_id_from_name(name).0)
    }

    /// Resolve a child mailbox of *this* actor, where the child is the
    /// instanced node `scope:segment` (ADR-0099 §3 — `scope` is the
    /// child's `NAMESPACE`, `segment` its `:` discriminator). The child's
    /// id folds that node's `ActorId` onto this actor's lineage carry,
    /// so a cap that owns a scoped-child facade — the component host
    /// reaching a loaded component, a socket listener reaching a session
    /// — composes the registered fold id without allocating a name.
    ///
    /// `self.mailbox` is the parent carry: exact for a root-pinned cap
    /// (depth-1, carry == id), which is every cap that hosts children.
    #[must_use]
    pub fn resolve_peer_scoped<Peer: Actor>(
        &self,
        scope: &str,
        segment: &str,
    ) -> FfiActorMailbox<Peer> {
        let node = ActorId::instanced(scope, segment);
        FfiActorMailbox::__new(with_tag(Tag::Mailbox, fold_lineage(self.mailbox, node)))
    }
}

impl<R: Actor> FfiActorMailbox<R> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>` — wrong-kind sends are rejected at
    /// the call site.
    ///
    /// Wire shape (cast or postcard) follows `Kind::encode_into_bytes`
    /// — same single source of truth as the kind-typed sends per
    /// issue #240.
    pub fn send<K>(&self, payload: &K)
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(self.mailbox, K::ID.0, &bytes, 1);
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only —
    /// see [`crate::actor::ctx::MailSender::send_many`] for the
    /// wire-shape rationale.
    pub fn send_many<K>(&self, payloads: &[K])
    where
        R: HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        MAIL_BRIDGE.send_mail(self.mailbox, K::ID.0, bytes, payloads.len() as u32);
    }
}
