//! Contract rows (ADR-0231 §1, §4): the type-level record of how a target
//! answers each kind it handles, plus the const list of those rows in the
//! manifest's own [`ReplyContract`] vocabulary.
//!
//! `#[actor]` emits one [`Contract<K>`] impl per handler and one
//! [`Contracts`] impl per actor, from the same parsed signature. A single or
//! deferred `-> O` handler's row is `O`, a `-> ()` handler's row is
//! [`Silent`], and a manual handler's row is [`Undeclared`]. A `#[fallback]`
//! contributes no row.
//!
//! The one [`Contracts`] impl carries the rows as a type-level list,
//! [`Contracts::Rows`], and each [`Contract<K>`] impl names its row's position
//! in it ([`Contract::Index`]), so a row exists only at a position of that list
//! (ADR-0231 §10).

use aether_data::{ActorMail, Kind, KindId, ReplyContract};

use super::declared::RowIndex;

/// The row of a handler that sends no reply (`-> ()`).
///
/// Never implements [`Kind`], which keeps it disjoint from the blanket
/// [`ReplyShape`] impl over every kind.
pub struct Silent;

/// The permanent row shape of a manual handler, whose replies are issued by
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
/// A protocol (ADR-0231 §2) has no `Contract` rows: it declares its rows as
/// [`Protocol::Rows`](crate::Protocol::Rows), and a target covers them through
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
pub trait Contract<K: Kind>: Contracts {
    /// The reply kind, [`Silent`], or [`Undeclared`].
    type Reply: ReplyShape;

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
