//! [`HandsOff`]: a [`Target`] a held reply may be handed to (ADR-0243 §9).

use aether_data::ActorMail;

use super::{ActorRef, Direct, ProtocolRef, Target};
use crate::model::{Anyone, Contract, HandlesKind, Protocol, RowAt};

mod sealed {
    use crate::reference::{ActorRef, ProtocolRef};

    /// The seal: no crate outside `aether-actor` can add an
    /// `impl HandsOff<K, R> for ErasedActorRef`, which `Target<K>` alone would
    /// admit and which would hand a held reply to an actor no row proves
    /// answers it.
    pub trait Sealed<K, R, I> {}

    impl<T, K, R> Sealed<K, R, super::Direct> for ActorRef<T> {}

    impl<P, K, R, I> Sealed<K, R, I> for ProtocolRef<P> {}

    impl<K, R, I, T: Sealed<K, R, I> + ?Sized> Sealed<K, R, I> for &T {}
}

/// A [`Target`] whose row for `K` answers exactly `R`, so a held `R` handed
/// to it with a `K` payload is answered by it with the `R` the requester
/// waits for (ADR-0243 §9).
///
/// An [`ActorRef<T>`] qualifies when `T`'s [`Contract<K>`] row replies `R`
/// and its handler asks nothing of its sender ([`Anyone`]): the hand-off is
/// made in the name of whichever actor holds the reply, which no bound here
/// names, so a handler that requires something of its sender is not a
/// hand-off target (ADR-0231 §11). A
/// [`ProtocolRef<P>`] when `P`'s row for `K` is [`Row<K, R>`](crate::Row), and
/// a borrow of either qualifies too. `R` is [`ActorMail`], so a [`Silent`] or
/// [`Undeclared`] row never qualifies, and an [`ErasedActorRef`] proves no
/// row, so it never qualifies either (#6895). Sealed: the one valid hand-off
/// target is a proven reference whose row answers the held kind.
///
/// [`Silent`]: crate::Silent
/// [`Undeclared`]: crate::Undeclared
/// [`ErasedActorRef`]: crate::ErasedActorRef
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot take a held `{R}` for `{K}`",
    label = "no row of this target answers `{K}` with `{R}` from any sender",
    note = "a held reply hands off to an `ActorRef<T>` whose handler for `{K}` replies `{R}` and takes no `sender` \
            parameter, or a `ProtocolRef<P>` whose row for `{K}` is `Row<{K}, {R}>`; an `ErasedActorRef` proves no \
            row (#6895)"
)]
pub trait HandsOff<K: ActorMail, R: ActorMail, I = Direct>: Target<K, I> + sealed::Sealed<K, R, I> {}

// Each impl is `do_not_recommend`, so a failed hand-off reports the
// `HandsOff` message rather than the reply equality or borrow it bounds.
#[diagnostic::do_not_recommend]
impl<T: HandlesKind<K> + Contract<K, Reply = R, Sender = Anyone>, K: ActorMail, R: ActorMail> HandsOff<K, R>
    for ActorRef<T>
{
}

#[diagnostic::do_not_recommend]
impl<P: Protocol, K: ActorMail, R: ActorMail, I> HandsOff<K, R, I> for ProtocolRef<P> where
    P::Rows: RowAt<K, I, Reply = R>
{
}

#[diagnostic::do_not_recommend]
impl<K: ActorMail, R: ActorMail, I, T: HandsOff<K, R, I> + ?Sized> HandsOff<K, R, I> for &T {}
