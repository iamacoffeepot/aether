//! The receive ctx's outbound surface — the inherent by-reference send
//! and the [`MailSender`] / [`OutboundReply`] / [`Emit`] impls on
//! [`WasmCtx`].

use aether_data::Kind;

use super::WasmCtx;
use crate::mail::ReplyHandle;
use crate::model::ctx::emit::Emit;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::outbound_reply::OutboundReply;
use crate::model::ctx::reply_mode::{Manual, Multi, ReplyMode};
use crate::model::{Addressable, CallerAddressable, CallerScoped, HandlesKind, Singleton};
use crate::reference::ErasedActorRef;
use crate::wasm::bridge::mail;
use crate::wasm::inline::ChainMode;

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// Issue 1987: send `payload` to a proven [`ErasedActorRef`], threading this
    /// actor's own id as the send's `from`. The untyped cell for a recipient
    /// known only at runtime takes the proof a spawn
    /// ([`InlineChild::erase`](super::InlineChild::erase),
    /// [`Self::spawn_inline_child_by_tag`]), a `child_as` / `sibling_as`
    /// lookup, or [`Self::sender`] produced — never a computed position
    /// (ADR-0230). There is no by-name counterpart, because text is not a
    /// proof. Routes through the inline registry and inherits the handler's
    /// causal chain like every ctx send.
    pub fn send_to<K: Kind>(&mut self, target: ErasedActorRef, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(target.id().0, K::ID.0, &bytes, 1, ChainMode::Inherit, self.mailbox);
    }
}

// ADR-0114 addressing amendment: every `WasmCtx` send resolves the recipient
// id then routes through the inline registry's `route_or_enqueue`, so a send
// to a cluster member (own id or a resident inline-child alias) dispatches in
// place through the membrane (queue + drain) and only a cross-cluster
// recipient hits the host. For a childless component with no captured
// `self_id` match the recipient is always `Remote`, so the path is identical
// to a bare `mail::send_mail`.
impl<A, M: ReplyMode> MailSender for WasmCtx<'_, A, M> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.inline.route_or_enqueue(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Detached,
            self.mailbox,
        );
    }

    // By-id detached send: the inherent `send_to` with `ChainMode::Detached`.
    fn send_detached_to<K: Kind>(&mut self, target: ErasedActorRef, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(target.id().0, K::ID.0, &bytes, 1, ChainMode::Detached, self.mailbox);
    }
}

// ADR-0112: the reply surface is per-mode. `Manual` carries it (a
// manual-class handler issues its own replies); `Single` deliberately
// does not, so a `-> ()` single handler is provably silent and a stray
// single-ctx `ctx.reply` is a compile error rather than a manifest lie.
impl<A> OutboundReply for WasmCtx<'_, A, Manual> {
    type ReplyHandle = ReplyHandle;

    fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender
    }

    fn reply<K: Kind>(&mut self, payload: &K) {
        if let Some(handle) = self.sender {
            let bytes = payload.encode_into_bytes();
            mail::reply_mail(handle.raw(), K::ID.0, &bytes, 1, self.mailbox);
        }
    }

    fn reply_to<K: Kind>(&mut self, sender: ReplyHandle, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::reply_mail(sender.raw(), K::ID.0, &bytes, 1, self.mailbox);
    }
}

// ADR-0134: the emit surface is the multi class's, implemented only for
// the `Multi<K>` mode. Each `emit` is `send_detached_to` at the proven
// `ctx.sender()` (a detached chain root addressed at the dispatch source),
// so an emission starts a fresh chain rather than holding the request
// chain open. A sourceless dispatch (session / broadcast / substrate-origin
// mail, `ctx.sender()` is `None`) has no routable target, so the emission
// warn-drops.
impl<A, K: Kind> Emit<K> for WasmCtx<'_, A, Multi<K>> {
    fn emit(&mut self, payload: &K) {
        let Some(target) = self.sender() else {
            tracing::warn!(
                kind = <K as Kind>::NAME,
                "multi handler emit dropped: the dispatch carries no routable \
                 source (session / broadcast / substrate-origin mail)",
            );
            return;
        };
        self.send_detached_to(target, payload);
    }
}
