//! The receive ctx's outbound surface — the inherent flat sends (to a
//! declared dependency, and through a held reference) and the
//! [`MailSender`] / [`OutboundReply`] impls on [`WasmCtx`].

use aether_data::{ActorMail, Kind, RequestId};

use super::WasmCtx;
use crate::blob::guest::{EncodedGuestMail, encode_guest};
use crate::mail::ReplyHandle;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::outbound_reply::OutboundReply;
use crate::model::ctx::reply_mode::{ReplyMode, Unchecked};
use crate::model::{
    Addressable, Anyone, CallerAddressable, CoveredBy, DependencyResolver, DependsOn, SendableTo, SentBy, Singleton,
};
use crate::reference::{ActorRef, ErasedActorRef, Target};
use crate::wasm::bridge::mail;
use crate::wasm::inline::{ChainMode, send_through_host};

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// Issue 1987: send `payload` through a held reference, threading this
    /// actor's own id as the send's `from` (ADR-0232 §1). The target is a
    /// [`Target`]: an [`ActorRef<R>`](crate::ActorRef) is kind-checked, so the
    /// send compiles only when `R` handles `K`, a
    /// [`ProtocolRef<P>`](crate::ProtocolRef) only for a kind `P` lists (its
    /// row index `I` inferred). An [`ErasedActorRef`] is not a target
    /// (ADR-0231 §4): a recipient known only at runtime, such as
    /// [`Self::sender`]'s, is cast once to a protocol with
    /// [`Self::cast`](super::WasmCtx::cast), and a spawned child is sent
    /// through its [`InlineChild`](super::InlineChild) or a reference it
    /// narrows to — never a computed position (ADR-0230). There is no
    /// by-name counterpart, because text is not a proof. Routes through the inline registry and inherits the handler's
    /// causal chain like every ctx send.
    ///
    /// The ctx's own actor must cover what the target's handler requires of
    /// its sender (ADR-0231 §11): nothing for most handlers, and the protocol
    /// `P` for one that takes `sender: ProtocolRef<P>`. The erased ctx names
    /// no actor, so it does not send such a kind.
    pub fn send_to<K: ActorMail, I, T: Target<K, I>>(&mut self, target: T, payload: &K)
    where
        T::Sender: CoveredBy<A>,
    {
        self.push(target.erased(), payload, ChainMode::Inherit);
    }

    /// Send `payload` to the declared dependency `R` (ADR-0232 §1–§2),
    /// inheriting the handler's causal chain (ADR-0080 §7).
    ///
    /// Compiles only on a ctx typed by an actor that declares `R` with
    /// `#[actor(depends(R))]`, and only for a kind `R` handles; the turbofish
    /// names only `R`, the kind is inferred from the payload. When `R`'s
    /// handler for the kind takes `sender: ProtocolRef<P>`, it compiles only
    /// when this actor has a handler for each of `P`'s kinds
    /// ([`SentBy<A, R>`](SentBy), ADR-0231 §11); the call site is the same
    /// line either way. The erased ctx has no flat send. The recipient is the
    /// position [`Self::actor_ref`] folds for `R`.
    ///
    /// Its consumers include the cube fixture's camera send.
    pub fn send<R: Singleton + CallerAddressable>(&mut self, payload: &(impl SendableTo<R> + SentBy<A, R>))
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push(self.actor_ref::<R>().erase(), payload, ChainMode::Inherit);
    }

    /// Send a slice of cast payloads to the declared dependency `R` as one
    /// contiguous batch, inheriting the handler's causal chain like
    /// [`Self::send`]. Cast-only: a structured kind has no efficient batched
    /// wire shape, so the payloads cross as one contiguous byte slice with
    /// their count.
    ///
    /// Its consumer is the cube fixture, which emits its twelve triangles as
    /// one batch.
    pub fn send_many<R: Singleton + CallerAddressable>(
        &mut self,
        payloads: &[impl SendableTo<R> + SentBy<A, R> + bytemuck::NoUninit],
    ) where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push_many(self.actor_ref::<R>().erase(), payloads);
    }

    /// Send a request to the declared dependency `R` and return the
    /// correlation id the host minted for it, inheriting the handler's causal
    /// chain like [`Self::send`]. A tracked send always goes through the host,
    /// even to a member of this actor's own inline cluster, so the id is always
    /// a real correlation its reply comes back on (ADR-0139).
    ///
    /// Its consumer is the fs demux fixture, which matches two
    /// indistinguishable `aether.fs.read` replies by these ids.
    #[must_use]
    pub fn send_tracked<R: Singleton + CallerAddressable>(
        &mut self,
        payload: &(impl SendableTo<R> + SentBy<A, R>),
    ) -> RequestId
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push_tracked(self.actor_ref::<R>(), payload)
    }

    /// Send a request to the declared dependency `R` and store `context`
    /// under the minted correlation id, for the reply handler to take back
    /// with [`Self::take_context`]. Inherits the handler's causal chain like
    /// [`Self::send`] and returns the minted id.
    ///
    /// The context moves into the table (ADR-0243 §4), so it may carry a
    /// [`Held`](crate::Held) reply: storing parks the ticket, and the reply
    /// handler's take claims it back. The send goes through the host even to
    /// a member of this actor's own inline cluster, so the id is always a real
    /// correlation and the context always reaches its reply.
    ///
    /// Its consumer is the fs demux fixture's context flow, whose two reads
    /// carry distinct typed contexts.
    #[must_use]
    pub fn send_with_context<R: Singleton + CallerAddressable>(
        &mut self,
        payload: &(impl SendableTo<R> + SentBy<A, R>),
        context: impl Kind,
    ) -> RequestId
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let request = self.push_tracked(self.actor_ref::<R>(), payload);
        self.inline.insert_request_context(request, context);
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
    pub fn send_detached<R: Singleton + CallerAddressable>(&mut self, payload: &(impl SendableTo<R> + SentBy<A, R>))
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        self.push(self.actor_ref::<R>().erase(), payload, ChainMode::Detached);
    }

    /// The one routing call every single-payload `WasmCtx` send funnels
    /// through: encode `payload` and hand it to the inline registry with
    /// `chain`, stamping this actor as the sender (issue 1987). A
    /// cluster-member recipient dispatches in place; any other hands off to
    /// the host (ADR-0114 addressing amendment).
    pub(crate) fn push<K: ActorMail>(&self, recipient: ErasedActorRef, payload: &K, chain: ChainMode) {
        self.inline.route_or_enqueue(recipient.id().0, K::ID.0, encode_guest(payload), 1, chain, self.mailbox);
    }

    /// The batch form of [`Self::push`]: the cast payloads cross as one
    /// contiguous slice with their count, inheriting the handler's chain.
    /// A cast payload cannot hold a `Blob`, so the batch keeps nothing.
    fn push_many<K: ActorMail + bytemuck::NoUninit>(&self, recipient: ErasedActorRef, payloads: &[K]) {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.inline.route_or_enqueue(
            recipient.id().0,
            K::ID.0,
            EncodedGuestMail::plain(bytes.to_vec()),
            payloads.len() as u32,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    /// [`Self::push`] with the chain inherited, returning the correlation id
    /// the host minted for the send. It skips the inline registry's in-place
    /// route: a cluster-member recipient would drain with no correlation and
    /// no reply handle, so its answer could never come back. Through the host
    /// the reply arrives as a correlated top-level dispatch (ADR-0114
    /// addressing amendment, ADR-0139).
    fn push_tracked<R: Addressable, K: ActorMail>(&self, recipient: ActorRef<R>, payload: &K) -> RequestId {
        send_through_host(recipient.id().0, K::ID.0, encode_guest(payload), 1, ChainMode::Inherit, self.mailbox);
        RequestId(mail::prev_correlation())
    }
}

// ADR-0114 addressing amendment: every `WasmCtx` send resolves the recipient
// id then routes through the inline registry's `route_or_enqueue`, so a send
// to a cluster member (own id or a resident inline-child alias) dispatches in
// place through the membrane (queue + drain) and only a cross-cluster
// recipient hits the host. A tracked send is the exception: `push_tracked`
// always goes through the host so its reply carries a correlation. For a
// childless component with no captured `self_id` match the recipient is
// always `Remote`, so the path is identical to a bare `mail::send_mail`.
impl<A, M: ReplyMode> MailSender for WasmCtx<'_, A, M> {
    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    // By-id detached send: the inherent `send_to` with `ChainMode::Detached`.
    fn send_detached_to<K: ActorMail, I>(&mut self, target: impl Target<K, I, Sender = Anyone>, payload: &K) {
        self.push(target.erased(), payload, ChainMode::Detached);
    }
}

// ADR-0112: the reply surface is per-mode. `Unchecked` carries it (a
// unchecked-class handler issues its own replies); `Single` deliberately
// does not, so a `-> ()` single handler is provably silent and a stray
// single-ctx `ctx.reply` is a compile error rather than a manifest lie.
impl<A> OutboundReply for WasmCtx<'_, A, Unchecked> {
    type ReplyHandle = ReplyHandle;

    fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender
    }

    fn reply<K: ActorMail>(&mut self, payload: &K) {
        if let Some(handle) = self.sender {
            let encoded = encode_guest(payload);
            mail::reply_mail(handle.raw(), K::ID.0, &encoded.bytes, 1, self.mailbox);
        }
    }

    fn reply_to<K: ActorMail>(&mut self, sender: ReplyHandle, payload: &K) {
        let encoded = encode_guest(payload);
        mail::reply_mail(sender.raw(), K::ID.0, &encoded.bytes, 1, self.mailbox);
    }
}
