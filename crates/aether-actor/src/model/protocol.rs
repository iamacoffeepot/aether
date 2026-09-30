//! Protocols (ADR-0231 §2, §6): a stable set of contract rows, independent of
//! any implementation, and the sealed check that a target covers them.
//!
//! A protocol is a type implementing [`Protocol`], whose [`Protocol::Rows`] is
//! a tuple of [`Row<K, O>`]s. Everything else is computed here from that tuple
//! through sealed traits: [`RowSet::CONTRACTS`] is the rows' list in the
//! manifest's [`ReplyContract`] vocabulary, [`CoversRows`] decides whether a
//! target has every row, [`CoveredBy`] is the protocol-side name of that
//! answer, and [`RowAt`] finds a kind's row for a send through a
//! [`ProtocolRef`](crate::ProtocolRef). A hand-written [`Protocol`] impl only
//! declares rows, so no impl can state a list, a coverage claim, or a sendable
//! kind that disagrees with them.
//!
//! `#[protocol]` on a trait of signatures is sugar for the declaration:
//!
//! ```ignore
//! #[protocol]
//! pub trait MeshLoader {
//!     fn load(mail: LoadMesh) -> MeshLoadResult;
//!     fn set_mode(mail: SetMode);
//! }
//!
//! // expands to
//! pub struct MeshLoader;
//!
//! impl Protocol for MeshLoader {
//!     type Rows = (Row<LoadMesh, MeshLoadResult>, Row<SetMode, Silent>);
//! }
//! ```
//!
//! A target covers a row only through a [`Contract<K>`] row with the same kind
//! and the exact reply type. A `#[fallback]` emits no row. A
//! `#[handler::unchecked(..)]` handler's row is [`Undeclared`], and covers only an
//! explicit unchecked protocol row with that same reply shape.

use core::marker::PhantomData;

use aether_data::{ActorMail, Kind, KindId, ReplyContract};

use super::contract::{Contract, ReplyShape, Silent, Undeclared};

/// One protocol row: kind `K`, answered with reply shape `O`: a reply kind,
/// [`Silent`], or [`Undeclared`].
///
/// A type-level label only, never constructed: a protocol names its rows as
/// the tuple [`Protocol::Rows`].
pub struct Row<K, O>(PhantomData<fn() -> (K, O)>);

mod reply_sealed {
    /// Private supertrait sealing [`super::RowReply`] to [`super::Silent`],
    /// [`super::Undeclared`], and every reply kind.
    pub trait Sealed {}

    impl Sealed for super::Silent {}
    impl Sealed for super::Undeclared {}
    impl<O: aether_data::ActorMail> Sealed for O {}
}

/// What a protocol row can name as its reply: a reply kind, [`Silent`], or
/// [`Undeclared`]. Sealed.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a protocol row's reply",
    label = "not a reply kind, `Silent`, or `Undeclared`",
    note = "a protocol row is single (`-> O`), silent (no return), or unchecked (`-> Undeclared`) (ADR-0231 §6)"
)]
pub trait RowReply: ReplyShape + reply_sealed::Sealed {}

impl RowReply for Silent {}
impl RowReply for Undeclared {}

// The failure a reader needs is the row reply, not the `ActorMail` bound
// this impl's where-clause adds.
#[diagnostic::do_not_recommend]
impl<O: ActorMail> RowReply for O {}

mod rows_sealed {
    /// Private supertrait sealing [`super::RowSet`] to the tuples of
    /// [`super::Row`]s this module implements it for.
    pub trait Sealed {}
}

/// A protocol's rows: a tuple `(Row<K1, O1>, …, Row<Kn, On>)` of one to 16
/// rows. Sealed, so the list below is always derived from the tuple.
///
/// Sixteen rows is the cap on one protocol.
pub trait RowSet: rows_sealed::Sealed {
    /// The rows as `(kind, reply)` pairs in the manifest's [`ReplyContract`]
    /// vocabulary, in tuple order: `One(O::ID)` for a row `O`, `None` for a
    /// [`Silent`] row, and `Unchecked` for an [`Undeclared`] row. The same mapping
    /// `#[actor]` uses for
    /// [`Contracts::CONTRACTS`](crate::Contracts::CONTRACTS), so the two lists
    /// compare directly.
    const CONTRACTS: &'static [(KindId, ReplyContract)];
}

mod covers_sealed {
    /// Private supertrait sealing [`super::CoversRows`] to the blanket impls
    /// this module emits per tuple arity.
    pub trait Sealed<Rows> {}
}

/// `T: CoversRows<Rows>` holds when `T` has a [`Contract<K>`] row with the
/// exact reply for every row in `Rows`. Sealed, and implemented by one blanket
/// per tuple arity, so this is the only place coverage is computed.
pub trait CoversRows<Rows>: covers_sealed::Sealed<Rows> {}

mod row_at_sealed {
    /// Private supertrait sealing [`super::RowAt`] to the per-position impls
    /// this module emits per tuple arity.
    pub trait Sealed<K, I> {}
}

/// The zero-sized index of a row in a protocol's [`Rows`](Protocol::Rows)
/// tuple: `At<0>` is the first row. The compiler infers it at a send through
/// a [`ProtocolRef`](crate::ProtocolRef) and no one writes it.
pub struct At<const N: usize>;

/// `Rows: RowAt<K, I>` holds when the row tuple `Rows` has a row for the kind
/// `K`, at the position `I`, and [`Reply`](RowAt::Reply) is that row's reply.
/// Sealed, and implemented once per tuple arity and position as
/// `RowAt<K_j, At<j>, Reply = O_j>`.
///
/// A send through a [`ProtocolRef<P>`](crate::ProtocolRef) is bounded
/// `P::Rows: RowAt<K, I>`, so it compiles only for a kind `P` lists, and the
/// compiler infers `I`. A protocol lists each kind once (`#[protocol]` refuses
/// a duplicate), so at most one position matches and the index is never
/// ambiguous.
#[diagnostic::on_unimplemented(
    message = "the protocol rows `{Self}` have no row for the kind `{K}`",
    label = "not a kind the protocol lists",
    note = "a protocol reference sends only the kinds its protocol lists (ADR-0231 §3)"
)]
pub trait RowAt<K, I>: row_at_sealed::Sealed<K, I> {
    /// The reply the row at this position names: a reply kind, [`Silent`], or
    /// [`Undeclared`].
    type Reply: RowReply;
}

/// Emits the `RowSet`, `CoversRows`, and `RowAt` impls for every tuple arity
/// from the full parameter list down to one row, peeling one row per step.
macro_rules! row_tuples {
    () => {};
    ($head_kind:ident $head_reply:ident $(, $kind:ident $reply:ident)*) => {
        row_tuples!(@impl $head_kind $head_reply $(, $kind $reply)*);
        row_tuples!($($kind $reply),*);
    };
    // One `RowAt` impl per position of the full tuple `$all`: each step takes
    // the next row and the next position literal.
    (@row_at [$($all_kind:ident $all_reply:ident),+] [$($index:literal)*]) => {};
    (
        @row_at [$($all_kind:ident $all_reply:ident),+] [$index:literal $($indices:literal)*]
        $kind:ident $reply:ident $(, $rest_kind:ident $rest_reply:ident)*
    ) => {
        impl<$($all_kind: Kind, $all_reply: RowReply),+> row_at_sealed::Sealed<$kind, At<$index>>
            for ($(Row<$all_kind, $all_reply>,)+)
        {
        }

        impl<$($all_kind: Kind, $all_reply: RowReply),+> RowAt<$kind, At<$index>> for ($(Row<$all_kind, $all_reply>,)+) {
            type Reply = $reply;
        }

        row_tuples!(@row_at [$($all_kind $all_reply),+] [$($indices)*] $($rest_kind $rest_reply),*);
    };
    (@impl $($kind:ident $reply:ident),+) => {
        row_tuples!(@row_at [$($kind $reply),+] [0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15] $($kind $reply),+);

        impl<$($kind: Kind, $reply: RowReply),+> rows_sealed::Sealed for ($(Row<$kind, $reply>,)+) {}

        impl<$($kind: Kind, $reply: RowReply),+> RowSet for ($(Row<$kind, $reply>,)+) {
            const CONTRACTS: &'static [(KindId, ReplyContract)] =
                &[$((<$kind as Kind>::ID, <$reply as ReplyShape>::CONTRACT)),+];
        }

        impl<T, $($kind: Kind, $reply: RowReply),+> covers_sealed::Sealed<($(Row<$kind, $reply>,)+)> for T
        where
            $(T: Contract<$kind, Reply = $reply>),+
        {
        }

        impl<T, $($kind: Kind, $reply: RowReply),+> CoversRows<($(Row<$kind, $reply>,)+)> for T
        where
            $(T: Contract<$kind, Reply = $reply>),+
        {
        }
    };
}

row_tuples!(
    K1 O1, K2 O2, K3 O3, K4 O4, K5 O5, K6 O6, K7 O7, K8 O8,
    K9 O9, K10 O10, K11 O11, K12 O12, K13 O13, K14 O14, K15 O15, K16 O16
);

/// A protocol (ADR-0231 §2): a type naming a stable set of contract rows.
///
/// Usually written with `#[protocol]` on a trait of signatures. A hand-written
/// impl is safe: it only declares [`Rows`](Protocol::Rows), and the rows'
/// list ([`RowSet::CONTRACTS`]) and who covers them ([`CoveredBy`]) are
/// derived from that declaration.
pub trait Protocol {
    /// The rows, a tuple of [`Row<K, O>`]s.
    type Rows: RowSet;
}

mod covered_sealed {
    /// Private supertrait sealing [`super::CoveredBy`] to its blanket impl.
    pub trait Sealed<R> {}
}

/// `P: CoveredBy<R>` holds when the target `R` has every row of the protocol
/// `P`: for each [`Row<K, O>`], a [`Contract<K>`] row replying exactly `O`.
///
/// Rows match by kind and exact reply type, never by method name. A silent row
/// is covered only by a silent handler, a row `O` only by a handler that
/// replies `O` directly or deferred, and an [`Undeclared`] row only by a
/// unchecked handler. A kind the target handles only through `#[fallback]` has
/// no row.
///
/// Sealed: the one impl is the blanket over [`Protocol`] and [`CoversRows`].
#[diagnostic::on_unimplemented(
    message = "`{R}` does not cover the protocol `{Self}`",
    label = "`{R}` lacks a row of `{Self}`, or `{Self}` is not a protocol",
    note = "a target covers a protocol with a handler for each of its kinds replying exactly the row's reply; \
            a `#[fallback]` has no row"
)]
pub trait CoveredBy<R>: covered_sealed::Sealed<R> {}

impl<P: Protocol, R: CoversRows<P::Rows>> covered_sealed::Sealed<R> for P {}

impl<P: Protocol, R: CoversRows<P::Rows>> CoveredBy<R> for P {}

pub(super) mod cast_sealed {
    /// Private supertrait sealing [`super::CastTarget`] to the protocols
    /// this crate lists: [`Subscriber<K>`](crate::Subscriber), and every
    /// [`ProtocolCast`](super::ProtocolCast) protocol under the one exact-rows
    /// rule [`super::covers_exact_rows`].
    pub trait Sealed {}
}

/// A protocol `ctx.cast` may type an erased reference as (ADR-0231 §4's
/// guard cast), on a native ctx and on a guest ctx alike. Sealed.
///
/// The cast reads the contract the reference's `Live` route published and
/// mints a [`ProtocolRef<Self>`](crate::ProtocolRef) only when
/// [`admits`](CastTarget::admits) accepts those rows. The rule lives here,
/// beside the protocol, and the seal keeps any other crate from naming a
/// protocol whose rule admits rows of its own choosing.
///
/// Two arms implement it: [`Subscriber<K>`](crate::Subscriber), whose silent
/// subscription rule also admits an unchecked row for its published kind, and
/// every protocol
/// `#[protocol]` declares (through [`ProtocolCast`]), whose rule is ADR-0231
/// §4's exact-rows check.
pub trait CastTarget: Protocol + cast_sealed::Sealed {
    /// Whether a route publishing `rows` answers this protocol.
    fn admits(rows: &[(KindId, ReplyContract)]) -> bool;
}

/// Opts a protocol into the guard cast's protocol arm (ADR-0231 §4).
/// `#[protocol]` emits it; not part of the public API.
///
/// The marker carries no rule of its own: a protocol that has it is cast by
/// [`covers_exact_rows`] alone, so a hand-written marker still gets only the
/// fixed exact-rows check against registry-published rows.
#[doc(hidden)]
pub trait ProtocolCast: Protocol {}

/// Whether `rows` contains every row of `P`, each with the same kind and the
/// same [`ReplyContract`] (ADR-0231 §4). A [`ReplyContract::Unchecked`] row
/// covers exactly an explicit unchecked protocol row, and extra rows are ignored.
fn covers_exact_rows<P: Protocol>(rows: &[(KindId, ReplyContract)]) -> bool {
    <P::Rows as RowSet>::CONTRACTS.iter().all(|row| rows.contains(row))
}

impl<P: ProtocolCast> cast_sealed::Sealed for P {}

impl<P: ProtocolCast> CastTarget for P {
    fn admits(rows: &[(KindId, ReplyContract)]) -> bool {
        covers_exact_rows::<P>(rows)
    }
}

#[cfg(test)]
mod tests {
    use aether_data::{Kind, ReplyContract};
    use aether_kinds::{Ping, Pong, Tick};

    use super::{CastTarget, ProtocolCast};
    use crate::{Protocol, Row, Silent, Undeclared};

    struct PingPong;

    impl Protocol for PingPong {
        type Rows = (Row<Ping, Pong>, Row<Pong, Silent>);
    }

    impl ProtocolCast for PingPong {}

    struct ManualPing;

    impl Protocol for ManualPing {
        type Rows = (Row<Ping, Undeclared>,);
    }

    impl ProtocolCast for ManualPing {}

    struct MixedPing;

    impl Protocol for MixedPing {
        type Rows = (Row<Ping, Undeclared>, Row<Pong, Silent>);
    }

    impl ProtocolCast for MixedPing {}

    // A cast that minted a protocol reference for a route missing one of the
    // protocol's rows, or answering one with another reply or by hand, would
    // hand its holder a target that warn-drops or misanswers that row.
    #[test]
    fn a_protocol_cast_admits_only_every_row_with_its_exact_reply() {
        let admits = |rows: &[_]| <PingPong as CastTarget>::admits(rows);
        let request = (Ping::ID, ReplyContract::One(Pong::ID));
        let notice = (Pong::ID, ReplyContract::None);

        assert!(admits(&[notice, request]));
        assert!(admits(&[(Tick::ID, ReplyContract::None), request, notice]));
        assert!(!admits(&[request]));
        assert!(!admits(&[(Ping::ID, ReplyContract::None), notice]));
        assert!(!admits(&[request, (Pong::ID, ReplyContract::Unchecked)]));
        assert!(!admits(&[(Ping::ID, ReplyContract::Unchecked), notice]));
    }

    // An unchecked protocol row must be admitted by the same exact-row rule as
    // every other protocol row. Treating Unchecked as silence or a wildcard
    // would mint a proof for a route whose response contract differs, while
    // checking only one row would mint an incomplete mixed protocol.
    #[test]
    fn a_manual_protocol_cast_requires_the_exact_manual_row_and_every_other_row() {
        let request = (Ping::ID, ReplyContract::Unchecked);
        let notice = (Pong::ID, ReplyContract::None);

        assert!(<ManualPing as CastTarget>::admits(&[request]));
        assert!(<ManualPing as CastTarget>::admits(&[(Tick::ID, ReplyContract::None), request]));
        assert!(!<ManualPing as CastTarget>::admits(&[]));
        assert!(!<ManualPing as CastTarget>::admits(&[(Ping::ID, ReplyContract::None)]));
        assert!(!<ManualPing as CastTarget>::admits(&[(Ping::ID, ReplyContract::One(Pong::ID))]));
        assert!(!<MixedPing as CastTarget>::admits(&[request]));
        assert!(<MixedPing as CastTarget>::admits(&[notice, request]));
    }
}
