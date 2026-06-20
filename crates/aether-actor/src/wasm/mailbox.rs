// Wire-encode: `usize → u32` narrowings forward batch lengths to the
// wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! [`WasmActorMailbox`] — actor-typed sender handle for FFI guests.
//!
//! Issue 665 split the prior parametric `ActorMailbox<'a, R, T>` into
//! per-side types so the `MailTransport` trait can retire. Issue 1987
//! made the FFI variant a ctx-bound transient (`WasmActorMailbox<'a, R>`),
//! symmetric with the native `NativeActorMailbox<'a, R>`: it carries the
//! resolving actor's own id as the send's "from" half plus a borrow of
//! the per-component inline registry the send routes through. The `'a`
//! borrow keeps origin a property of the executing actor — the handle
//! cannot be stored past the handler, so it can never carry a stale
//! origin.
//!
//! Built via [`crate::wasm::ctx::WasmCtx::actor`] /
//! [`crate::wasm::ctx::WasmCtx::resolve_actor`]. The compile-time
//! `R: HandlesKind<K>` gate is the same as the prior parametric form:
//! `ctx.actor::<RenderCapability>().send(&triangle)` compiles only when
//! `RenderCapability: HandlesKind<DrawTriangle>`.

use core::marker::PhantomData;

use aether_data::{ActorId, Kind, Tag, fold_lineage, with_tag};

use crate::actor::{Addressable, HandlesKind};
use crate::wasm::inline::InlineRegistry;

/// Phantom-typed receiver-actor handle for FFI guests, built by
/// [`crate::wasm::WasmCtx::actor`] / [`crate::wasm::WasmCtx::resolve_actor`].
///
/// Issue 1987 made it a ctx-bound transient (mirroring the native
/// `NativeActorMailbox<'a, R>` and the in-cluster [`crate::wasm::RelativeMailbox`]):
/// it carries the resolving actor's own folded id as the `sender` (the "from"
/// half every send stamps as origin) plus a borrow of the per-component inline
/// registry the send routes through. The `'a` borrow is what keeps origin a
/// property of the *executing* actor — the handle cannot outlive the handler,
/// so it can never carry a stale origin the way a stored address-only token
/// would.
pub struct WasmActorMailbox<'a, R> {
    mailbox: u64,
    /// The resolving actor's own folded [`aether_data::MailboxId`] raw value —
    /// the "from" half threaded onto every send so the recipient's
    /// `ctx.source_mailbox()` resolves who sent it, and so the host stamps the
    /// correct origin without an ambient per-receive cell (issue 1987). Set by
    /// the ctx-level constructors to the resolving ctx's own id.
    sender: u64,
    /// The per-component inline registry the send routes through
    /// ([`InlineRegistry::route_or_enqueue`]): a cluster-member recipient
    /// dispatches in place, any other recipient hands off to the host. A typed
    /// peer / cap recipient is always cross-cluster, so this resolves to the
    /// host send — the registry borrow only keeps the routing path uniform with
    /// the in-cluster [`crate::wasm::RelativeMailbox`].
    inline: &'a InlineRegistry,
    _r: PhantomData<fn() -> R>,
}

impl<R> Copy for WasmActorMailbox<'_, R> {}
impl<R> Clone for WasmActorMailbox<'_, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, R> WasmActorMailbox<'a, R> {
    /// Not part of the public API; the ctx-level constructors go
    /// through here so the fields stay private. `sender` is the
    /// resolving actor's own id (the "from" half); `inline` is the ctx's
    /// per-component inline registry the send routes through.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64, sender: u64, inline: &'a InlineRegistry) -> Self {
        Self {
            mailbox,
            sender,
            inline,
            _r: PhantomData,
        }
    }

    /// The receiver's typed mailbox id. Exposed for callers that need
    /// it for diagnostics or a host fn the SDK doesn't yet wrap.
    #[must_use]
    pub fn mailbox_id(&self) -> aether_data::MailboxId {
        aether_data::MailboxId(self.mailbox)
    }

    /// Rewrap a precomputed `mailbox` id as a typed peer handle that
    /// inherits this handle's ctx binding (`sender` + inline registry), so
    /// the rewrapped handle's sends stamp the same origin and route the
    /// same way. The by-id counterpart of [`Self::resolve_peer_scoped`], for
    /// a cap that folds a child / session id itself rather than resolving by
    /// name.
    #[must_use]
    pub fn at<Peer>(&self, mailbox: u64) -> WasmActorMailbox<'a, Peer> {
        WasmActorMailbox::__new(mailbox, self.sender, self.inline)
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
    pub fn resolve_peer_scoped<Peer: Addressable>(
        &self,
        scope: &str,
        segment: &str,
    ) -> WasmActorMailbox<'a, Peer> {
        let node = ActorId::instanced(scope, segment);
        WasmActorMailbox::__new(
            with_tag(Tag::Mailbox, fold_lineage(self.mailbox, node)),
            self.sender,
            self.inline,
        )
    }
}

impl<R: Addressable> WasmActorMailbox<'_, R> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>` — wrong-kind sends are rejected at
    /// the call site.
    ///
    /// Threads the resolving actor's own id as the send's `from`
    /// (issue 1987): the host stamps it as origin (validated in-cluster),
    /// so the recipient's `ctx.source_mailbox()` resolves the sender with
    /// no ambient host cell. Inherits the handler's in-flight causal
    /// chain by default (ADR-0080 §7): the host stamps the dispatch's
    /// `parent`/`root` onto this send, so the recipient's work settles
    /// back into the caller's chain. Reach for [`Self::send_detached`]
    /// for the rare fire-and-forget send that should start its own chain.
    ///
    /// Wire shape (cast or structured) follows `Kind::encode_into_bytes`
    /// — same single source of truth as the kind-typed sends per
    /// issue #240.
    pub fn send<K>(&self, payload: &K)
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline
            .route_or_enqueue(self.mailbox, K::ID.0, &bytes, 1, false, self.sender);
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only —
    /// see [`crate::actor::ctx::MailSender::send_many`] for the
    /// wire-shape rationale. Inherits the handler's causal chain like
    /// [`Self::send`].
    pub fn send_many<K>(&self, payloads: &[K])
    where
        R: HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.inline.route_or_enqueue(
            self.mailbox,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            false,
            self.sender,
        );
    }

    /// ADR-0080 §7 fire-and-forget escape hatch: send `payload` to `R`
    /// without inheriting the handler's in-flight causal chain. The
    /// host mints a fresh root, so the recipient processes the mail as
    /// the start of a new tree.
    ///
    /// **Fire-and-forget only.** A detached send mints no parent
    /// linkage, so any reply the recipient issues inherits the
    /// *recipient's* tree rather than the sender's. Reply-correlated
    /// requests always go through [`Self::send`].
    pub fn send_detached<K>(&self, payload: &K)
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline
            .route_or_enqueue(self.mailbox, K::ID.0, &bytes, 1, true, self.sender);
    }
}
