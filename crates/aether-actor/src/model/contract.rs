//! Contract rows (ADR-0231 §1, §4): the type-level record of how a target
//! answers each kind it handles, plus the const list of those rows in the
//! manifest's own [`ReplyContract`] vocabulary.
//!
//! `#[actor]` emits one [`Contract<K>`] impl per handler and one
//! [`Contracts`] impl per actor, from the same parsed signature. A single or
//! deferred `-> O` handler's row is `O`, a `-> ()` handler's row is
//! [`Silent`], and an unchecked handler's row is [`Undeclared`]. A `#[fallback]`
//! contributes no row.
//!
//! The one [`Contracts`] impl carries the rows as a type-level list,
//! [`Contracts::Rows`], and each [`Contract<K>`] impl names its row's position
//! in it ([`Contract::Index`]), so a row exists only at a position of that list
//! (ADR-0231 §10).
//!
//! The watch traits (ADR-0079 §8) live here because a watch is declared the
//! way a row is: [`Watches<W>`] is emitted by `#[actor]` from a departure
//! handler's signature and requires the actor's row for the notice, so
//! `ctx.watch` compiles only for a watched type the actor has a handler for.

use aether_data::{ActorId, ActorMail, Kind, KindId, MailboxId, ReplyContract, fnv1a_64_bytes, fnv1a_64_fold};
use aether_kinds::MonitorNotice;

use super::declared::RowIndex;
use super::protocol::{Protocol, RowSet};
use super::{Addressable, HandlesKind};
use crate::reference::{ActorRef, ProtocolRef};

/// The row of a handler that sends no reply (`-> ()`).
///
/// Never implements [`Kind`], which keeps it disjoint from the blanket
/// [`ReplyShape`] impl over every kind.
pub struct Silent;

/// The permanent row shape of an unchecked handler, whose replies are issued by
/// hand and so have no statically declared kind (ADR-0231 §6). A protocol may
/// name this shape to promise that it handles a kind without promising how it
/// responds.
///
/// Never implements [`Kind`], which keeps it disjoint from the blanket
/// [`ReplyShape`] impl over every kind.
pub struct Undeclared;

mod sealed {
    /// Private supertrait sealing [`super::ReplyShape`] to the three impls in
    /// this module, so the set of row shapes is closed.
    pub trait Sealed {}

    impl Sealed for super::Silent {}
    impl Sealed for super::Undeclared {}
    impl<O: aether_data::ActorMail> Sealed for O {}
}

/// What a contract row can name as its reply: a reply kind, [`Silent`], or
/// [`Undeclared`]. Sealed. A reply kind must be [`ActorMail`], so a handler
/// that declares an engine-only reply (ADR-0233) fails at its declaration.
pub trait ReplyShape: sealed::Sealed {
    /// The row in the manifest's vocabulary, as the inputs manifest and the
    /// native `HandlerEntry` report it.
    const CONTRACT: ReplyContract;
}

impl ReplyShape for Silent {
    const CONTRACT: ReplyContract = ReplyContract::None;
}

impl ReplyShape for Undeclared {
    const CONTRACT: ReplyContract = ReplyContract::Unchecked;
}

impl<O: ActorMail> ReplyShape for O {
    const CONTRACT: ReplyContract = ReplyContract::One(O::ID);
}

mod silent_sealed {
    /// Private supertrait sealing [`super::SilentRow`] to [`super::Silent`]
    /// and [`super::Undeclared`], so no reply kind can be marked silent.
    pub trait Sealed {}

    impl Sealed for super::Silent {}
    impl Sealed for super::Undeclared {}
}

/// A row that answers a published event with no typed reply (ADR-0231 §8):
/// [`Silent`], or an unchecked handler's [`Undeclared`]. Sealed.
///
/// The flat [`ctx.subscribe::<P, K>()`](crate::WasmCtx::subscribe) verb
/// requires the subscriber's [`Contract<K>`] row to be one, because a
/// publisher's broadcast has no one waiting for a reply.
#[diagnostic::on_unimplemented(
    message = "a subscriber's handler for a published kind replies `{Self}`",
    note = "a published event has no one waiting for a reply; the handler must return `()` (ADR-0231 §8)"
)]
pub trait SilentRow: ReplyShape + silent_sealed::Sealed {}

impl SilentRow for Silent {}
impl SilentRow for Undeclared {}

/// The contract row a target has for `K`: `T: Contract<K, Reply = O>` means
/// `T` handles `K` and answers it with `O`, with nothing ([`Silent`]), or by
/// hand ([`Undeclared`]).
///
/// A protocol (ADR-0231 §2) has no `Contract` rows: it declares its rows as
/// [`Protocol::Rows`], and a target covers them through
/// these rows ([`CoveredBy`](crate::CoveredBy)).
///
/// A row exists only at a position of the target's one
/// [`Contracts::Rows`] list (ADR-0231 §10): [`Index`](Contract::Index) names
/// that position, and its bound holds only when the list holds a
/// [`Row<K, Reply>`](crate::Row) there. `#[actor]` writes the list and each
/// row's position from the handlers it parses, so a hand-written row for a
/// kind the actor has no handler for either repeats an emitted impl (`E0119`)
/// or names a position that holds another kind or none (`E0277`).
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no contract row for `{K}`",
    label = "no handler for `{K}` on this target",
    note = "a `#[fallback]` does not count as handling a kind"
)]
pub trait Contract<K: Kind>: Contracts + HandlesKind<K, Sender = <Self as Contract<K>>::Sender> {
    /// The reply kind, [`Silent`], or [`Undeclared`].
    type Reply: ReplyShape;

    /// What the row's handler requires of its sender: the protocol its ctx
    /// names as its sender, or [`Anyone`](super::Anyone) when it names none.
    /// The supertrait bound holds it equal to
    /// [`HandlesKind::Sender`], which the typed sends read, so a row cannot
    /// state one requirement to a send and another to a protocol's coverage.
    /// Coverage reads it here, so a target that lacks a row is reported once.
    type Sender: Protocol;

    /// The row's position in [`Contracts::Rows`]: [`Here`](crate::Here) for
    /// the first handler, [`There<I>`](crate::There) past it. Written by the
    /// expansion that declared the handler.
    #[doc(hidden)]
    type Index: RowIndex<<Self as Contracts>::Rows, K, Reply = Self::Reply>;
}

/// Every contract row of a target, as the type-level list its
/// [`Contract<K>`] rows index into ([`Rows`](Contracts::Rows)) and as
/// `(kind, reply)` pairs in the manifest's [`ReplyContract`] vocabulary, so
/// the list compares directly with a loaded target's handler rows
/// (ADR-0231 §4).
pub trait Contracts {
    /// `(Row<K1, O1>, (Row<K2, O2>, (…, ())))`: one entry per handler, in
    /// declaration order, then an adopted native handler set's rows. A
    /// `#[cfg]`-gated handler whose predicates are off keeps its slot as
    /// [`Gap`](crate::Gap), so every other row's position is the same in every
    /// configuration (ADR-0231 §10).
    type Rows;

    /// One entry per [`Contract`] row, handler-set rows included.
    const CONTRACTS: &'static [(KindId, ReplyContract)];
}

mod watch_sealed {
    use aether_data::MailboxId;

    /// Private supertrait sealing [`super::WatchTarget`] to the two typed
    /// references, and the only door to what the SDK needs from one: its
    /// position, the tag of the type it was watched through, and a reference
    /// of the same type at a departed actor's position. The module is
    /// private, so no other crate can call these.
    pub trait Sealed: Copy {
        /// The position the reference proves.
        fn position(self) -> MailboxId;

        /// The tag the host keys a watch through this reference type by. A
        /// predecessor and its successor compute the same tag.
        fn watched_tag() -> u64;

        /// The reference a departure event carries: this reference type at
        /// `position`, where the host recorded a watch made through it.
        fn departed_at(position: MailboxId) -> Self;
    }
}

pub use watch_sealed::Sealed as WatchedRef;

/// A type an actor can be watched through (ADR-0079 §8): an actor type,
/// watched through an [`ActorRef`], or a protocol, watched through a
/// [`ProtocolRef`].
///
/// `#[actor]` emits it for every actor and `#[protocol]` for every protocol;
/// [`Subscriber<K>`](crate::Subscriber) has it by hand. A departure handler names the watched
/// type as `Departed<W>`, and its event carries a [`Ref`](Watchable::Ref) at
/// the departed actor's position.
pub trait Watchable: Sized {
    /// The typed reference an actor is watched through as this type:
    /// `ActorRef<Self>` for an actor type, `ProtocolRef<Self>` for a
    /// protocol.
    type Ref: WatchTarget<Watched = Self>;
}

/// A reference `ctx.watch` takes (ADR-0079 §8): an [`ActorRef<R>`] or a
/// [`ProtocolRef<P>`]. Sealed.
///
/// An [`ErasedActorRef`](crate::ErasedActorRef) is not one: a watch's event
/// is typed by what was watched, so a reference known only at run time, such
/// as `ctx.sender()`, is cast to a protocol first with `ctx.cast`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a reference `ctx.watch` takes",
    note = "watch through an `ActorRef<R>` or a `ProtocolRef<P>`; cast an `ErasedActorRef` to a protocol first \
            with `ctx.cast` (ADR-0079 §8)"
)]
pub trait WatchTarget: watch_sealed::Sealed {
    /// The type the target is watched through, which picks the handler its
    /// departure runs.
    type Watched: Watchable<Ref = Self>;
}

impl<R: Addressable + Watchable<Ref = Self>> watch_sealed::Sealed for ActorRef<R> {
    fn position(self) -> MailboxId {
        self.id()
    }

    /// The actor type's tag, the fold of its `NAMESPACE`.
    fn watched_tag() -> u64 {
        ActorId::singleton(R::NAMESPACE).0
    }

    fn departed_at(position: MailboxId) -> Self {
        Self::new(position)
    }
}

impl<R: Addressable + Watchable<Ref = Self>> WatchTarget for ActorRef<R> {
    type Watched = R;
}

/// Domain of a protocol's watched-type tag, so it shares no preimage with an
/// actor type's namespace fold.
const PROTOCOL_WATCH_DOMAIN: &[u8] = b"aether.watch.protocol:";

impl<P: Protocol + Watchable<Ref = Self>> watch_sealed::Sealed for ProtocolRef<P> {
    fn position(self) -> MailboxId {
        self.erase().id()
    }

    /// A protocol has no name: it is its rows (ADR-0231 §2), so its tag is a
    /// fold of them. Two protocols with the same rows are one watched type,
    /// as they are one cast target.
    fn watched_tag() -> u64 {
        <P::Rows as RowSet>::CONTRACTS.iter().fold(fnv1a_64_bytes(PROTOCOL_WATCH_DOMAIN), |hash, (kind, reply)| {
            let (shape, reply_kind) = match reply {
                ReplyContract::None => (0u8, 0u64),
                ReplyContract::One(reply_kind) => (1, reply_kind.0),
                ReplyContract::Unchecked => (3, 0),
            };
            let hash = fnv1a_64_fold(hash, &kind.0.to_le_bytes());
            fnv1a_64_fold(fnv1a_64_fold(hash, &[shape]), &reply_kind.to_le_bytes())
        })
    }

    fn departed_at(position: MailboxId) -> Self {
        Self::new(position)
    }
}

impl<P: Protocol + Watchable<Ref = Self>> WatchTarget for ProtocolRef<P> {
    type Watched = P;
}

/// `A: Watches<W>` holds when the actor `A` has a departure handler for the
/// watched type `W` (ADR-0079 §8): a `#[handler::event]` whose third
/// parameter is `Departed<W>`. `#[actor]` emits it from that handler, and
/// [`Context`](Watches::Context) is the kind the handler takes as its fourth
/// parameter, or `NoContext` when it takes none.
///
/// `ctx.watch` is bounded by it, so a watch through a type no handler names
/// does not compile, and neither does one whose context is another kind.
/// Every watch of one watched type therefore shares one context kind; an
/// actor that keeps notes of different shapes about actors of one type
/// declares an enum kind for them.
///
/// The context is [`ActorMail`], which a kind holding a held reply is not
/// (ADR-0242): a released watch's context is dropped without being decoded,
/// which would strand a held ticket.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no departure handler for `{W}`",
    label = "a watch through this type would never be handled",
    note = "add `#[handler::event] fn on_gone(&mut self, ctx: &mut WasmCtx<'_>, event: Departed<{W}>, context: C)` \
            (ADR-0079 §8)"
)]
pub trait Watches<W: Watchable>: Contract<MonitorNotice> {
    /// The context every watch of `W` stores and its handler is handed.
    type Context: ActorMail;
}

#[cfg(test)]
mod watch_tests {
    use aether_kinds::{Ping, Pong, Tick};

    use super::{Watchable, WatchedRef};
    use crate::model::protocol::{Protocol, Row};
    use crate::model::{Silent, Subscriber};
    use crate::reference::ProtocolRef;

    struct Asks;

    impl Protocol for Asks {
        type Rows = (Row<Ping, Pong>,);
    }

    impl Watchable for Asks {
        type Ref = ProtocolRef<Self>;
    }

    struct Hears;

    impl Protocol for Hears {
        type Rows = (Row<Ping, Silent>,);
    }

    impl Watchable for Hears {
        type Ref = ProtocolRef<Self>;
    }

    fn tag<W: Watchable>() -> u64 {
        <W::Ref as WatchedRef>::watched_tag()
    }

    // The host picks a departure's handler by this tag. A fold that ignored
    // a row's reply, or its kind, would hand one protocol's departure to
    // another protocol's handler; one that differed between two spellings of
    // the same rows would lose a watch carried across a republish.
    #[test]
    fn a_protocols_watched_tag_follows_its_rows() {
        assert_ne!(tag::<Asks>(), tag::<Hears>(), "the reply is part of the row");
        assert_ne!(tag::<Subscriber<Ping>>(), tag::<Subscriber<Tick>>(), "the kind is part of the row");
        assert_eq!(tag::<Hears>(), tag::<Subscriber<Ping>>(), "two protocols with the same rows are one watched type");
    }
}
