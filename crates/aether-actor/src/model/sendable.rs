//! [`SendableTo`]: the payload bound of the flat typed send verbs
//! (ADR-0232 §2).

use aether_data::ActorMail;

use super::HandlesKind;

mod sealed {
    use aether_data::ActorMail;

    use crate::model::HandlesKind;

    /// The seal: only the blanket impl below makes a kind sendable, so no
    /// crate outside `aether-actor` can mark a kind sendable to an actor that
    /// does not handle it.
    pub trait Sealed<R> {}

    impl<K: ActorMail, R: HandlesKind<K>> Sealed<R> for K {}
}

/// A kind that may be mailed to actor `R`: `K: SendableTo<R>` holds exactly
/// when `R: HandlesKind<K>`.
///
/// The flat typed verbs (`ctx.send::<R>(&kind)` and its siblings) take the
/// payload as `&impl SendableTo<R>`, so the kind is inferred from the argument
/// and the turbofish names only the recipient. The check lives on the payload
/// because the recipient's own markers ([`HandlesKind`],
/// [`Contract`](super::Contract)) are implemented on the target, not on the
/// kind, and Rust has no partial turbofish that could leave `K` to inference.
///
/// Sealed: the one blanket impl is the only way in, so the bound refuses a
/// kind the recipient has no handler for rather than letting it warn-drop at
/// run time.
///
/// The supertrait is [`ActorMail`], so an engine-only kind (ADR-0233) is
/// refused at the call site even when the recipient handles it:
///
/// ```compile_fail
/// fn requires<K: aether_data::ActorMail>() {}
/// requires::<aether_kinds::MonitorNotice>();
/// ```
///
/// An ordinary kind satisfies the same bound:
///
/// ```
/// fn requires<K: aether_data::ActorMail>() {}
/// requires::<aether_kinds::Ping>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{R}` has no handler for `{Self}`",
    label = "`{R}` does not handle `{Self}`",
    note = "a `#[fallback]` does not count as handling a kind"
)]
pub trait SendableTo<R>: ActorMail + sealed::Sealed<R> {}

impl<K: ActorMail, R: HandlesKind<K>> SendableTo<R> for K {}

mod sent_sealed {
    use aether_data::Kind;

    use crate::model::{CoveredBy, HandlesKind};

    /// The seal: only the blanket impl below admits a sender.
    pub trait Sealed<A, R> {}

    impl<K: Kind, A, R: HandlesKind<K>> Sealed<A, R> for K where R::Sender: CoveredBy<A> {}
}

/// A kind the actor `A` may send to `R`: `K: SentBy<A, R>` holds when `A`
/// covers what `R`'s handler for `K` requires of its sender.
#[diagnostic::on_unimplemented(
    message = "`{A}` may not send `{Self}` to `{R}`",
    label = "`{R}`'s handler for `{Self}` requires its sender to handle kinds `{A}` does not",
    note = "the handler names the protocol in its `sender: ProtocolRef<P>` parameter; add a handler for each of `P`'s kinds"
)]
pub trait SentBy<A, R>: sent_sealed::Sealed<A, R> {}

impl<K: aether_data::Kind, A, R: HandlesKind<K>> SentBy<A, R> for K where R::Sender: super::CoveredBy<A> {}
