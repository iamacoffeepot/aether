//! The receive ctx's outbound surface — the inherent flat sends (to a
//! declared dependency, and through a held reference) and the
//! [`MailSender`] / [`OutboundReply`] impls on [`WasmCtx`].

use aether_data::{ActorMail, Kind, MailboxId, RequestId, Source};

use super::WasmCtx;
use crate::mail::ReplyHandle;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::outbound_reply::OutboundReply;
use crate::model::ctx::reply_mode::{Manual, ReplyMode};
use crate::model::{CallerAddressable, DependencyResolver, DependsOn, SendableTo, Singleton};
use crate::reference::{ErasedActorRef, Target};
use crate::wasm::bridge::mail;
use crate::wasm::inline::{ChainMode, RouteDecision};

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// Issue 1987: send `payload` through a held reference, threading this
    /// actor's own id as the send's `from` (ADR-0232 §1). The target is a
    /// [`Target`]: an [`ActorRef<R>`](crate::ActorRef) is kind-checked, so the
    /// send compiles only when `R` handles `K`, and an [`ErasedActorRef`] is
    /// not. The erased proof for a recipient known only at runtime is one a
    /// spawn ([`InlineChild::erase`](super::InlineChild::erase),
    /// [`Self::spawn_inline_child_by_tag`]), a `child_as` / `sibling_as`
    /// lookup, or [`Self::sender`] produced — never a computed position
    /// (ADR-0230). There is no by-name counterpart, because text is not a
    /// proof. Routes through the inline registry and inherits the handler's
    /// causal chain like every ctx send.
    pub fn send_to<K: ActorMail>(&mut self, target: impl Target<K>, payload: &K) {
        self.push(target.erased().id().0, payload, ChainMode::Inherit);
    }

    /// Send `payload` to the declared dependency `R` (ADR-0232 §1–§2),
    /// inheriting the handler's causal chain (ADR-0080 §7).
    ///
    /// Compiles only on a ctx typed by an actor that declares `R` with
    /// `#[actor(depends(R))]`, and only for a kind `R` handles; the turbofish
    /// names only `R`, the kind is inferred from the payload. The erased ctx
    /// has no flat send. The recipient is the position [`Self::actor_ref`]
    /// folds for `R`.
    ///
    /// Its consumers are the puppet motors' pose sends (`aether.puppet-idle`,
    /// `aether.puppet-turntable`) and the cube fixture's camera send.
    pub fn send<R: Singleton + CallerAddressable>(&mut self, payload: &impl SendableTo<R>)
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push(self.actor_ref::<R>().id().0, payload, ChainMode::Inherit);
    }

    /// Send a slice of cast payloads to the declared dependency `R` as one
    /// contiguous batch, inheriting the handler's causal chain like
    /// [`Self::send`]. Cast-only: a structured kind has no efficient batched
    /// wire shape, so the payloads cross as one contiguous byte slice with
    /// their count.
    ///
    /// Its consumer is the cube fixture, which emits its twelve triangles as
    /// one batch.
    pub fn send_many<R: Singleton + CallerAddressable>(&mut self, payloads: &[impl SendableTo<R> + bytemuck::NoUninit])
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push_many(self.actor_ref::<R>().id().0, payloads);
    }

    /// Send a request to the declared dependency `R` and return the
    /// correlation id the host minted for it, inheriting the handler's causal
    /// chain like [`Self::send`]. An inline-cluster local route has no host
    /// correlation, so it warn-logs and returns the no-correlation sentinel.
    ///
    /// Its consumer is the fs demux fixture, which matches two
    /// indistinguishable `aether.fs.read` replies by these ids.
    #[must_use]
    pub fn send_tracked<R: Singleton + CallerAddressable>(&mut self, payload: &impl SendableTo<R>) -> RequestId
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push_tracked(self.actor_ref::<R>().id().0, payload)
    }

    /// Send a request to the declared dependency `R` and store `context`
    /// under the minted correlation id, for the reply handler to take back
    /// with [`Self::take_context`]. Inherits the handler's causal chain like
    /// [`Self::send`] and returns the minted id.
    ///
    /// Its consumer is the fs demux fixture's context flow, whose two reads
    /// carry distinct typed contexts.
    #[must_use]
    pub fn send_with_context<R: Singleton + CallerAddressable>(
        &mut self,
        payload: &impl SendableTo<R>,
        context: &impl Kind,
    ) -> RequestId
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let request = self.push_tracked(self.actor_ref::<R>().id().0, payload);
        if request.0 != Source::NO_CORRELATION {
            self.inline.insert_request_context(request, context);
        }
        request
    }

    /// Send `payload` to the declared dependency `R` on a fresh causal chain,
    /// ignoring this handler's in-flight lineage (ADR-0080 §7, ADR-0232 §1–§2).
    ///
    /// Compiles only on a ctx typed by an actor that declares `R` with
    /// `#[actor(depends(R))]`, and only for a kind `R` handles; the turbofish
    /// names only `R`, the kind is inferred from the payload. The recipient is
    /// the position [`Self::actor_ref`] folds for `R`, as for [`Self::send`]. A
    /// send the running chain caused inherits it through [`Self::send`]
    /// instead.
    ///
    /// **Fire-and-forget only.** A detached send mints no parent linkage, so
    /// any reply the recipient issues roots in the recipient's tree rather
    /// than the sender's.
    ///
    /// Its consumer is the routed HTTP fixture's drop bridge, which keeps the
    /// component teardown out of the request's causal chain.
    pub fn send_detached<R: Singleton + CallerAddressable>(&mut self, payload: &impl SendableTo<R>)
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push(self.actor_ref::<R>().id().0, payload, ChainMode::Detached);
    }

    /// The one routing call every single-payload `WasmCtx` send funnels
    /// through: encode `payload` and hand it to the inline registry with
    /// `chain`, stamping this actor as the sender (issue 1987). A
    /// cluster-member recipient dispatches in place; any other hands off to
    /// the host (ADR-0114 addressing amendment).
    fn push<K: ActorMail>(&self, recipient: u64, payload: &K, chain: ChainMode) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(recipient, K::ID.0, &bytes, 1, chain, self.mailbox);
    }

    /// The batch form of [`Self::push`]: the cast payloads cross as one
    /// contiguous slice with their count, inheriting the handler's chain.
    fn push_many<K: ActorMail + bytemuck::NoUninit>(&self, recipient: u64, payloads: &[K]) {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.inline.route_or_enqueue(
            recipient,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    /// [`Self::push`] with the chain inherited, returning the correlation id
    /// the host minted for the send. An inline-cluster local send never
    /// leaves the guest, so no host correlation exists for it: that path
    /// warn-logs and returns the no-correlation sentinel rather than reading
    /// a stale `prev_correlation_p32` value.
    fn push_tracked<K: ActorMail>(&self, recipient: u64, payload: &K) -> RequestId {
        match self.inline.route_decision(recipient) {
            RouteDecision::Local => {
                self.push(recipient, payload, ChainMode::Inherit);
                tracing::warn!(
                    kind = <K as Kind>::NAME,
                    recipient = self.inline.subname_of(MailboxId(recipient)).as_deref(),
                    "send_tracked on an inline-cluster local route has no host correlation",
                );
                RequestId(Source::NO_CORRELATION)
            }
            RouteDecision::Remote => {
                self.push(recipient, payload, ChainMode::Inherit);
                RequestId(mail::prev_correlation())
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
impl<A, M: ReplyMode> MailSender for WasmCtx<'_, A, M> {
    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    // By-id detached send: the inherent `send_to` with `ChainMode::Detached`.
    fn send_detached_to<K: ActorMail>(&mut self, target: ErasedActorRef, payload: &K) {
        self.push(target.id().0, payload, ChainMode::Detached);
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

    fn reply<K: ActorMail>(&mut self, payload: &K) {
        if let Some(handle) = self.sender {
            let bytes = payload.encode_into_bytes();
            mail::reply_mail(handle.raw(), K::ID.0, &bytes, 1, self.mailbox);
        }
    }

    fn reply_to<K: ActorMail>(&mut self, sender: ReplyHandle, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::reply_mail(sender.raw(), K::ID.0, &bytes, 1, self.mailbox);
    }
}
