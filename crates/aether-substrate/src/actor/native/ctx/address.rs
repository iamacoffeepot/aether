//! Who this ctx is, and who it can address.
//!
//! "Who it can address" is answered by proof. [`NativeCtx::actor_ref`] mints
//! one for a declared dependency, [`NativeCtx::resolve_live`] proves a
//! position that arrived in a payload, [`NativeCtx::resolve_path`] proves
//! an [`ErasedActorPath`] that arrived in one, [`NativeCtx::resolve`]
//! proves a [`ProtocolPath`] that arrived in one, and [`NativeCtx::cast`]
//! types an erased reference already held as a protocol; each hands back a proven
//! reference, which is what ADR-0230 lets a cap keep past the handler that
//! received it and what the flat send verbs route through. [`NativeCtx::accept_bundle`] is
//! the bundle front of the payload-borne door: it proves a mail bundle's
//! addresses and hands back items that can only be delivered.
//! [`NativeCtx::accept_call`] is its one-item form for a wire `Call`, whose
//! recipient is an [`ErasedActorPath`] proven on arrival.
//!
//! Beside these doors sit `outbound_parent` and `outbound_root`, the lineage
//! every inheriting send stamps so it joins the handler's causal chain
//! (ADR-0080 §7).

use std::error::Error;
use std::fmt;

use aether_actor::{
    ActorRef, Addressable, CallerAddressable, CallerScoped, CastTarget, DependencyResolver, DependsOn, ErasedActorRef,
    Protocol, ProtocolPath, ProtocolRef, ReplyMode, ResolveError, Singleton,
};
use aether_data::{ErasedActorPath, KindId, MailId, MailboxId};
use aether_kinds::NamedMail;

use crate::mail::registry::{AddressResolutionError, Registry, ResolveLiveError};
use crate::mail::{BoundaryMail, boundary};

use super::NativeCtx;

/// Why an [`ErasedActorPath`] that arrived in a payload could not be proven (ADR-0230 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvePathError {
    /// The registry's own refusal: an unknown or instanced root, an illegal or
    /// ambiguous segment, an over-cap path, or no route at the canonical path.
    Unresolved(AddressResolutionError),
    /// The path names a route whose actor is not `Live` (its birth is still `Starting`).
    NotLive { canonical_path: String },
}

impl fmt::Display for ResolvePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolved(error) => write!(formatter, "{error}"),
            Self::NotLive { canonical_path } => write!(formatter, "{canonical_path} is not live"),
        }
    }
}

impl Error for ResolvePathError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unresolved(error) => Some(error),
            Self::NotLive { .. } => None,
        }
    }
}

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// Proven reference to a declared dependency (ADR-0230): mints an
    /// [`ActorRef`] for the position `R`'s resolver folds beneath this
    /// binding's scope, with no registry read — the load was refused unless `R` was `Live`, so the
    /// answer is already known. Bounded `A: DependsOn<R>` directly, so it
    /// does not exist on the erased ctx. The one caller of the registry's
    /// `declared_dependency` mint.
    #[must_use]
    pub fn actor_ref<R: Singleton + CallerAddressable>(&self) -> ActorRef<R>
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        Registry::declared_dependency(R::resolve(
            self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE),
            (),
        ))
    }

    /// Prove a position that arrived in a payload (ADR-0230): the third door
    /// onto a proven reference on this ctx.
    ///
    /// The other two ask nothing of the registry. [`Self::send_to`] sends
    /// through a proof the actor already holds, and [`Self::actor_ref`] mints a
    /// declared dependency's proof from an answer the load already gave. This
    /// one is for a position that reached the actor without a proof, which
    /// nothing upstream proved, so it pays one published-route read to find
    /// out. It runs once, at receipt, and never on the send path; a caller
    /// that proves a position here keeps the proof, not the position. An
    /// actor carried in a payload is named by a path instead, and proven
    /// through [`Self::resolve_path`] or [`Self::resolve`].
    ///
    /// [`Self::sender`](super::NativeCtx::sender) remains the door for the
    /// host-stamped source — that answer is already known and costs no read.
    ///
    /// This is the only spelling a capability uses: no ctx door reaches the
    /// registry, so a cap cannot own a second answer to the registry's own
    /// liveness question that could drift from this one.
    pub fn resolve_live(&self, position: MailboxId) -> Result<ErasedActorRef, ResolveLiveError> {
        self.binding.mailer().registry().resolve_live(position)
    }

    /// Prove an [`ErasedActorPath`] that arrived in a payload: the address front of
    /// [`Self::resolve_live`]. The host's `resolve_address` expands and
    /// resolves the path — ADR-0166 short-path expansion and canonical
    /// validation are the registry's own — and the answered position is
    /// proven at once; it never leaves the verb.
    ///
    /// # Errors
    ///
    /// [`ResolvePathError::Unresolved`] with the registry's refusal when the
    /// path resolves to no route, and [`ResolvePathError::NotLive`] naming
    /// the canonical path when its route is not `Live`. Neither names a
    /// position.
    ///
    /// Its consumers are the component host's drop, replace, load-under, and
    /// describe receipts, the trampoline's replacement dependency check, and
    /// the HTTP server's `unregister_route` receipt, which needs only the
    /// identity its route table is keyed by.
    pub fn resolve_path(&self, address: &ErasedActorPath) -> Result<ErasedActorRef, ResolvePathError> {
        self.binding.resolve_path(address)
    }

    /// Prove a [`ProtocolPath<P>`] that arrived in mail (ADR-0231 §3): the
    /// protocol-typed front of [`Self::resolve_path`].
    ///
    /// The path is canonical, so it is folded as written, and one route-table
    /// read finds the `Live` route under exactly that canonical name. The
    /// [`ProtocolRef<P>`] it mints proves liveness, and sends through
    /// `send_to` for exactly the kinds `P` lists; `P` is the path's claim.
    ///
    /// # Errors
    ///
    /// [`ResolveError::NotLive`] naming the path when no `Live` route stands
    /// under its canonical name. It never names a position.
    ///
    /// Its consumers are the Bloomery workspace's receipt of `Run.source` and
    /// `Import.source` (#6841), the window manager's and the lifecycle
    /// capability's explicit subscribe and unsubscribe receipts, whose
    /// subscriber is a `ProtocolPath<Subscriber<K>>`, the HTTP server's
    /// `register_route` receipt, whose handler is a `ProtocolPath<HttpRouter>`,
    /// and the tcp capability's `connect` and `bind_listener` receipts, whose
    /// consumer is a `ProtocolPath<TcpConsumer>`.
    pub fn resolve<P: Protocol>(&self, path: &ProtocolPath<P>) -> Result<ProtocolRef<P>, ResolveError> {
        self.binding.mailer().registry().resolve_protocol(path)
    }

    /// Type an erased reference this actor already holds as the protocol `T`
    /// (ADR-0231 §4's guard cast): `Some` when the reference's route is
    /// `Live` and the rows it published answer `T`, `None` otherwise.
    ///
    /// One read of the published view answers both. The reference usually
    /// arrived untyped, as [`Self::sender`](super::NativeCtx::sender) does, and
    /// the cast is how a handler that must send through it later gets a typed
    /// proof to keep. `T` is sealed to two arms:
    /// [`Subscriber<K>`](aether_actor::Subscriber) admits a silent or manual
    /// row for `K`, and a `#[protocol]` type admits a route that publishes
    /// every one of its rows with the exact reply (ADR-0231 §4's protocol
    /// arm).
    ///
    /// Its consumers are the window manager's and the lifecycle capability's
    /// reflexive `subscribe_self` receipts, which type the sender as a
    /// subscriber to the requested kind and refuse one whose rows do not
    /// answer it, and the tcp capability's `connect_self` and
    /// `bind_listener_self` receipts, which type the sender as a
    /// `TcpConsumer` and refuse one that does not cover it, and the HTTP
    /// server's `register_route_self` receipt, which types the sender as an
    /// `HttpRouter`.
    #[must_use]
    pub fn cast<T: CastTarget>(&self, reference: ErasedActorRef) -> Option<ProtocolRef<T>> {
        self.binding.mailer().registry().cast(reference)
    }

    /// Prove a mail bundle that crossed the MCP or harness boundary inside a
    /// payload (ADR-0230 §3): the bundle front of [`Self::resolve_live`].
    ///
    /// Every item's [`ErasedActorPath`] recipient resolves
    /// and is proven before any item is returned, so a refusal — an absent,
    /// ambiguous, dropped, or still-starting recipient, or an unknown kind —
    /// moves no mail; the error names the recipient and `label`. The items
    /// come back as [`BoundaryMail`]s, which can only be delivered, through
    /// [`Self::deliver_detached`] or [`Self::deliver_forwarded`].
    ///
    /// Its consumers are `aether.trace`'s `DispatchTraced` and
    /// `aether.render`'s `CaptureFrame`, which proves both of its bundles
    /// before either moves.
    pub fn accept_bundle(&self, bundle: Vec<NamedMail>, label: &str) -> Result<Vec<BoundaryMail>, String> {
        boundary::accept(self.boundary_registry(), bundle, label)
    }

    /// Prove a wire `Call`'s recipient on arrival (ADR-0230 §3): the one-item
    /// form of [`Self::accept_bundle`].
    ///
    /// `recipient` is the [`ErasedActorPath`] the `Call` named. It resolves against
    /// this engine, so a short path expands against this engine's
    /// declarations, and the answered position is proven at once. A path that
    /// does not resolve to a `Live` actor — never registered, still starting,
    /// dropped, or an ambiguous or illegal short path — is refused with the
    /// registry's diagnostic, and nothing moves. `kind` is taken as given.
    ///
    /// The item that comes back can only be delivered, through
    /// [`Self::deliver_detached`]; the proof never leaves it, so the caller
    /// holds no reference it could keep or send another kind through.
    ///
    /// Its consumer is `RpcServerCapability`'s `Call` receipt, which answers a
    /// refusal as `RpcError::NotPresent`.
    pub fn accept_call(
        &self,
        recipient: &ErasedActorPath,
        kind: KindId,
        payload: Vec<u8>,
    ) -> Result<BoundaryMail, String> {
        boundary::accept_call(self.boundary_registry(), recipient, kind, payload)
    }

    /// The host registry the two boundary fronts, [`Self::accept_bundle`] and
    /// [`Self::accept_call`], prove their paths against. Private: it serves
    /// those two fronts and no cap reaches the registry through it.
    fn boundary_registry(&self) -> &Registry {
        self.binding.mailer().registry()
    }

    /// ADR-0080 §5: derive the `parent_mail` to stamp on outbound
    /// mail from this ctx's in-flight context: `None` for a chassis-root
    /// or close/init ctx.
    pub(crate) fn outbound_parent(&self) -> Option<MailId> {
        self.in_flight_mail_id
    }

    /// ADR-0080 §5: derive the inherited `root` to stamp on outbound
    /// mail from this ctx's in-flight context. `None` when there is none,
    /// in which case `NativeBinding::push_envelope_buffered` mints a fresh
    /// root from the outbound's own `mail_id`.
    pub(crate) fn outbound_root(&self) -> Option<MailId> {
        self.in_flight_root
    }
}
