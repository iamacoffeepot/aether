//! Contract rows (ADR-0231 §1, §4): the type-level record of how a target
//! answers each kind it handles, plus the const list of those rows in the
//! manifest's own [`ReplyContract`] vocabulary.
//!
//! `#[actor]` emits one [`Contract<K>`] impl per handler and one
//! [`Contracts`] impl per actor, from the same parsed signature. A single or
//! deferred `-> O` handler's row is `O`, a `-> ()` handler's row is
//! [`Silent`], and a manual handler's row is [`Undeclared`]. A `#[fallback]`
//! contributes no row.

use aether_data::{ActorMail, Kind, KindId, ReplyContract};

/// The row of a handler that sends no reply (`-> ()`).
///
/// Never implements [`Kind`], which keeps it disjoint from the blanket
/// [`ReplyShape`] impl over every kind.
pub struct Silent;

/// The row of a manual handler, whose replies are issued by hand and so have
/// no statically declared kind. Transitional (ADR-0231 §6): it goes away once
/// every manual handler declares its reply.
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
    const CONTRACT: ReplyContract = ReplyContract::Manual;
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
/// [`Silent`], or a manual handler's [`Undeclared`]. Sealed.
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
/// No `Addressable` supertrait: a protocol (ADR-0231 §2) carries rows too.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no contract row for `{K}`",
    label = "no handler for `{K}` on this target",
    note = "a `#[fallback]` does not count as handling a kind"
)]
pub trait Contract<K: Kind> {
    /// The reply kind, [`Silent`], or [`Undeclared`].
    type Reply: ReplyShape;
}

/// Every contract row of a target as `(kind, reply)` pairs, in the
/// manifest's [`ReplyContract`] vocabulary, so the list compares directly
/// with a loaded target's handler rows (ADR-0231 §4).
pub trait Contracts {
    /// One entry per [`Contract`] row, handler-set rows included.
    const CONTRACTS: &'static [(KindId, ReplyContract)];
}
