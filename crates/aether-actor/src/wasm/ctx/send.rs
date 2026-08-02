//! The receive ctx's outbound surface — the inherent by-token / by-id sends
//! and the [`MailSender`] / [`OutboundReply`] / [`Emit`] impls on
//! [`WasmCtx`].

use aether_data::{Kind, MailboxId, RequestId, Source, mailbox_id_from_name};

use super::WasmCtx;
use crate::mail::ReplyHandle;
use crate::mail::mailbox::Mailbox;
use crate::model::ctx::emit::Emit;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::outbound_reply::OutboundReply;
use crate::model::ctx::reply_mode::{Manual, Multi, ReplyMode};
use crate::model::{HandlesKind, Singleton};
use crate::wasm::bridge::mail;
use crate::wasm::inline::{ChainMode, RouteDecision};

impl<M: ReplyMode> WasmCtx<'_, M> {
    /// Issue 1987: send `payload` through a stored [`Mailbox<K>`] addressing
    /// token, threading this actor's own id as the send's `from` so the
    /// recipient's `ctx.source_mailbox()` resolves the sender and the host
    /// stamps the correct origin. A `Mailbox<K>` is a pure address (it
    /// carries no origin), so the ctx supplies the "from" half — the
    /// by-token counterpart of `ctx.actor::<R>().send(&k)`. Routes through
    /// the inline registry like every ctx send: a cluster-member recipient
    /// dispatches in place, any other hands off to the host. Inherits the
    /// handler's in-flight causal chain (ADR-0080 §7).
    pub fn send<K: Kind>(&mut self, mailbox: Mailbox<K>, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(mailbox.mailbox(), K::ID.0, &bytes, 1, ChainMode::Inherit, self.mailbox);
    }

    /// Send through a stored mailbox token and store a typed context for the
    /// reply correlation id.
    #[must_use]
    pub fn send_with_context<K: Kind, C: Kind>(&mut self, mailbox: Mailbox<K>, payload: &K, context: &C) -> RequestId {
        match self.inline.route_decision(mailbox.mailbox()) {
            RouteDecision::Local => {
                tracing::warn!(
                    kind = K::NAME,
                    recipient = mailbox.mailbox(),
                    "send_with_context on an inline-cluster local route has no host correlation",
                );
                self.send(mailbox, payload);
                RequestId(Source::NO_CORRELATION)
            }
            RouteDecision::Remote => {
                self.send(mailbox, payload);
                let request = RequestId(mail::prev_correlation());
                // SAFETY: the macro-emitted registry is accessed only under the
                // serialized wasm guest entrypoint.
                unsafe {
                    self.inline.request_contexts_mut().insert(request, context);
                }
                request
            }
        }
    }

    /// Issue 1987: send `payload` to a raw [`MailboxId`], threading this
    /// actor's own id as the send's `from`. The by-id escape hatch for a
    /// recipient address known only at runtime (the typed-token counterpart
    /// is [`Self::send`]; the by-name counterpart is
    /// [`MailSender::send_to_named`]). Routes through the
    /// inline registry and inherits the handler's causal chain like every
    /// ctx send.
    pub fn send_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(id.0, K::ID.0, &bytes, 1, ChainMode::Inherit, self.mailbox);
    }

    /// Send to a raw mailbox id and store a typed context for the reply
    /// correlation id.
    #[must_use]
    pub fn send_to_with_context<K: Kind, C: Kind>(&mut self, id: MailboxId, payload: &K, context: &C) -> RequestId {
        match self.inline.route_decision(id.0) {
            RouteDecision::Local => {
                tracing::warn!(
                    kind = K::NAME,
                    recipient = id.0,
                    "send_to_with_context on an inline-cluster local route has no host correlation",
                );
                self.send_to(id, payload);
                RequestId(Source::NO_CORRELATION)
            }
            RouteDecision::Remote => {
                self.send_to(id, payload);
                let request = RequestId(mail::prev_correlation());
                // SAFETY: the macro-emitted registry is accessed only under the
                // serialized wasm guest entrypoint.
                unsafe {
                    self.inline.request_contexts_mut().insert(request, context);
                }
                request
            }
        }
    }
}

// ADR-0114 addressing amendment: every `WasmCtx` send resolves the recipient
// id then routes through the inline registry's `route_or_enqueue`, so a send
// to a cluster member (own id or a resident inline-child alias) dispatches in
// place through the membrane (queue + drain) and only a cross-cluster
// recipient hits the host. For a childless component with no captured
// `self_id` match the recipient is always `Remote`, so the path is identical
// to a bare `mail::send_mail`.
impl<M: ReplyMode> MailSender for WasmCtx<'_, M> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            R::resolve(self.mailbox, ()).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.inline.route_or_enqueue(
            R::resolve(self.mailbox, ()).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `MailSender::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            mailbox_id_from_name(name).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    //noinspection DuplicatedCode
    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            R::resolve(self.mailbox, ()).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Detached,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name detached escape hatch — the `send_to_named` counterpart.
    #[allow(clippy::disallowed_methods)]
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            mailbox_id_from_name(name).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Detached,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // By-id detached send: the inherent `send_to` with `ChainMode::Detached`.
    fn send_detached_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(id.0, K::ID.0, &bytes, 1, ChainMode::Detached, self.mailbox);
    }
}

// ADR-0112: the reply surface is per-mode. `Manual` carries it (a
// manual-class handler issues its own replies); `Single` deliberately
// does not, so a `-> ()` single handler is provably silent and a stray
// single-ctx `ctx.reply` is a compile error rather than a manifest lie.
impl OutboundReply for WasmCtx<'_, Manual> {
    type ReplyHandle = ReplyHandle;

    fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender
    }

    fn source_mailbox(&self) -> Option<MailboxId> {
        // Issue 2687: delegate to the inherent generic accessor (the single
        // source of truth), which the `Single` `#[fallback]` ctx also reads.
        // The fully-qualified path resolves the inherent method, not this
        // trait method, so there is no recursion.
        WasmCtx::source_mailbox(self)
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
// the `Multi<K>` mode. Each `emit` is a detached chain root addressed at
// the dispatch source (`self.source`, the `send_detached_to` body with the
// source as recipient), so an emission starts a fresh chain rather than
// holding the request chain open. A sourceless dispatch (session /
// broadcast / substrate-origin mail, `MailboxId::NONE`) has no routable
// target, so the emission warn-drops.
impl<K: Kind> Emit<K> for WasmCtx<'_, Multi<K>> {
    fn emit(&mut self, payload: &K) {
        if self.source == MailboxId::NONE.0 {
            tracing::warn!(
                kind = <K as Kind>::NAME,
                "multi handler emit dropped: the dispatch carries no routable \
                 source (session / broadcast / substrate-origin mail)",
            );
            return;
        }
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(self.source, K::ID.0, &bytes, 1, ChainMode::Detached, self.mailbox);
    }
}
