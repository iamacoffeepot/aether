//! The publisher markers: [`Publishes<K>`], which says an actor fans a kind
//! out to its subscribers, and [`Publisher`], which says how that actor's
//! subscribe and unsubscribe requests are built (ADR-0232 §3), and
//! [`Subscriber<K>`], the protocol a publisher holds each subscriber under
//! (ADR-0231 §8).

use core::marker::PhantomData;

use aether_data::{ActorMail, Kind, KindId, ReplyContract};

use super::protocol::{CastTarget, Row, cast_sealed};
use super::{Addressable, Protocol, SendableTo, Silent};

/// The one-row protocol of a subscriber to the published kind `K`: it
/// handles `K` silently (ADR-0231 §8).
///
/// A publisher holds each subscriber as a
/// [`ProtocolRef<Subscriber<K>>`](crate::ProtocolRef), so its fan-out
/// compiles only for `K`. An explicit subscribe request carries a
/// [`ProtocolPath<Subscriber<K>>`](crate::ProtocolPath), which decodes only
/// against a route that publishes the exact silent row. A reflexive request
/// types its sender with the native `ctx.cast`, which also admits an unchecked
/// row for `K`: the one place an unchecked handler passes as silent, because a
/// published event has no one waiting for a reply either way.
///
/// A type-level label only, never constructed.
pub struct Subscriber<K>(PhantomData<fn() -> K>);

impl<K: ActorMail> Protocol for Subscriber<K> {
    type Rows = (Row<K, Silent>,);
}

impl<K: ActorMail> cast_sealed::Sealed for Subscriber<K> {}

/// A route answers `Subscriber<K>` when it publishes a row for `K` that is
/// silent or unchecked. A replying row is refused, since the publisher would
/// drop every reply.
impl<K: ActorMail> CastTarget for Subscriber<K> {
    fn admits(rows: &[(KindId, ReplyContract)]) -> bool {
        rows.iter()
            .any(|&(kind, reply)| kind == K::ID && matches!(reply, ReplyContract::None | ReplyContract::Unchecked))
    }
}

/// How a publishing actor's subscription requests are built: one impl per
/// publisher, written in the publisher's own crate beside its
/// [`Publishes`] impls.
///
/// The flat [`ctx.subscribe::<P, K>()`](crate::WasmCtx::subscribe) /
/// [`ctx.unsubscribe::<P, K>()`](crate::WasmCtx::unsubscribe) verbs are its
/// consumer. Each sends the request the
/// matching builder returns to `P`, so the verb body is a plain flat send and
/// the publisher's request kinds and handlers stay its own.
///
/// The builders are bounded `where Self: Publishes<K>`, so a request is built
/// only for a kind the publisher publishes.
pub trait Publisher: Addressable {
    /// The request that adds the sending actor to this publisher's `K`
    /// subscriber set.
    type Subscribe: SendableTo<Self>;

    /// The request that removes the sending actor from this publisher's `K`
    /// subscriber set.
    type Unsubscribe: SendableTo<Self>;

    /// Build the request that subscribes the sender to `K`.
    fn subscribe_request<K: Kind>() -> Self::Subscribe
    where
        Self: Publishes<K>;

    /// Build the request that unsubscribes the sender from `K`.
    fn unsubscribe_request<K: Kind>() -> Self::Unsubscribe
    where
        Self: Publishes<K>;
}

/// Per-published-kind marker: `P: Publishes<K>` means actor `P` is a
/// source of kind `K` — it fans `K` out to whoever subscribed to it.
/// The send-side mirror of [`HandlesKind`](super::HandlesKind): that marker
/// says "this actor accepts `K` as mail," this one says "this actor emits `K`
/// to its subscribers."
///
/// Gates the flat `ctx.subscribe::<P, K>()` / `ctx.unsubscribe::<P, K>()`
/// verbs, so a subscription to a kind the cap never emits is an
/// `E0277` at the `wire` call site rather than a stored row that never
/// fires. Subscribing the wrong cap is otherwise silent end to end: the
/// row is accepted, the event is dropped at its source for want of a
/// matching subscriber, and the component simply looks dead.
///
/// Unlike [`HandlesKind`](super::HandlesKind), which the `#[actor]` macro
/// emits from the handler list, these impls are written by hand in the
/// publishing cap's crate. A cap's published vocabulary belongs to the
/// *mailbox*, not to any one runtime or backend behind it: `aether.window`
/// runs a desktop or a synthetic backend, and a chassis with no window
/// composes none, so only the identity callers address is the right place to
/// state what the mailbox emits.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not publish `{K}`",
    label = "subscribing here would store a row that never fires",
    note = "an event with no matching subscriber is dropped at its source, so a subscription on the wrong \
            capability fails silently at run time — it is refused here instead",
    note = "frame-lifecycle stages (`Tick`, `Render`, `Present`, `InitCaps`, `InitComponents`, `Shutdown`) come \
            from `LifecycleCapability`: `ctx.subscribe::<LifecycleCapability, Tick>()`",
    note = "window device events (`Key`, `KeyRelease`, `MouseMove`, `MouseButton`, `MouseButtonRelease`, \
            `MouseWheel`, `WindowSize`, `TextInput`, `ImePreedit`, `Modifiers`) and window lifecycle \
            (`WindowOpened`, `WindowClosed`, `WindowMenuActivated`) come from `WindowCapability`: \
            `ctx.subscribe::<WindowCapability, Key>()`"
)]
pub trait Publishes<K: Kind>: Publisher {}

#[cfg(test)]
mod tests {
    use aether_data::{Kind, ReplyContract};
    use aether_kinds::{Ping, Pong};

    use super::{CastTarget, Subscriber};

    // A cast that admitted a replying handler would hand a publisher a
    // subscriber whose every reply it drops, and one that admitted a route
    // with no row for the kind would fan the event out to an actor that
    // cannot handle it.
    #[test]
    fn a_subscriber_cast_admits_only_a_silent_or_manual_row_for_its_kind() {
        let admits = |rows: &[_]| <Subscriber<Ping> as CastTarget>::admits(rows);

        assert!(admits(&[(Pong::ID, ReplyContract::None), (Ping::ID, ReplyContract::None)]));
        assert!(admits(&[(Ping::ID, ReplyContract::Unchecked)]));
        assert!(!admits(&[(Ping::ID, ReplyContract::One(Pong::ID))]));
        assert!(!admits(&[(Pong::ID, ReplyContract::None)]));
        assert!(!admits(&[]));
    }
}
