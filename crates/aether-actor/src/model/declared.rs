//! Declaration lists and their positions (ADR-0231 §10): the one place an
//! actor's contract rows, declared dependencies, and declared inline children
//! are stated, and the sealed index each per-entry marker names into them.
//!
//! Coherence allows one impl of a trait for a type. `#[actor]` emits exactly
//! one [`Contracts`](crate::Contracts) impl and one [`Declared`] impl per
//! actor, and `export!` one `ListedModule` impl per module, each carrying its
//! entries as a type-level list `(E1, (E2, (…, ())))`. Every per-entry marker
//! ([`Contract<K>`](crate::Contract), [`DependsOn<R>`](crate::DependsOn),
//! [`Spawns<C>`](crate::Spawns), [`Rebuildable<M>`](crate::Rebuildable))
//! names the position of its entry in that list as `type Index`, and the
//! marker's `Index` bound holds only when that position holds that entry. A
//! hand-written marker either repeats an impl the expansion emitted (`E0119`)
//! or names a position that holds a different entry or none (`E0277`), so a
//! marker that compiles is backed by a declaration.
//!
//! A position is [`Here`] (the list's head) or [`There<I>`] (position `I` of
//! the tail). The expansion writes each entry's position; no one else needs
//! to. A contract row list keeps every handler's slot whatever the enabled
//! features: a `#[cfg]`-gated handler whose predicates are off fills its slot
//! with [`Gap`], which only [`There`] steps over, so every other handler's
//! position is the same in every configuration.

use core::marker::PhantomData;

use aether_data::Kind;

use super::contract::ReplyShape;
use super::protocol::Row;

/// The first position of a declaration list: its head.
///
/// A type-level label only, never constructed.
pub struct Here;

/// The position `I` of a declaration list's tail: one step past its head.
///
/// A type-level label only, never constructed.
pub struct There<I>(PhantomData<fn() -> I>);

/// The slot of a contract row whose handler's `#[cfg]`s are off in this
/// configuration. It holds no row, so no position names a kind at it, and
/// [`There`] steps over it like any other entry.
///
/// A type-level label only, never constructed.
pub struct Gap;

mod sealed {
    /// Private supertrait sealing [`super::ListIndex`] to its two structural
    /// impls.
    pub trait ListSealed<L, T: ?Sized> {}

    /// Private supertrait sealing [`super::RowIndex`] to its two structural
    /// impls.
    pub trait RowSealed<L, K> {}
}

/// `I: ListIndex<L, T>` holds when position `I` of the declaration list `L`
/// holds `T`. Sealed, and implemented only structurally: [`Here`] for a list
/// whose head is `T`, and [`There<I>`] for a list whose tail holds `T` at `I`.
#[diagnostic::on_unimplemented(
    message = "the position `{Self}` does not hold `{T}` in the declared list `{L}`",
    label = "no declaration backs this entry",
    note = "a dependency is declared in `depends(..)` and an inline child in `spawns(..)` on the actor's \
            `#[actor(..)]`, and a module's listing in its `export!`; only those expansions write these markers"
)]
pub trait ListIndex<L, T: ?Sized>: sealed::ListSealed<L, T> {}

impl<T, Tail> sealed::ListSealed<(T, Tail), T> for Here {}

impl<T, Tail> ListIndex<(T, Tail), T> for Here {}

impl<H, Tail, T, I: ListIndex<Tail, T>> sealed::ListSealed<(H, Tail), T> for There<I> {}

impl<H, Tail, T, I: ListIndex<Tail, T>> ListIndex<(H, Tail), T> for There<I> {}

/// `I: RowIndex<L, K>` holds when position `I` of the contract row list `L`
/// holds a [`Row<K, O>`], and names that row's reply `O` as
/// [`Reply`](RowIndex::Reply). Sealed, and implemented only structurally, as
/// [`ListIndex`] is. A [`Gap`] slot holds no row, so only [`There`] passes it.
#[diagnostic::on_unimplemented(
    message = "the position `{Self}` holds no contract row for `{K}` in `{L}`",
    label = "no handler backs this contract row",
    note = "a contract row exists only where the actor's `#[actor]` declares a handler for its kind (ADR-0231 §10)"
)]
pub trait RowIndex<L, K>: sealed::RowSealed<L, K> {
    /// The reply the row at this position names.
    type Reply: ReplyShape;
}

impl<K: Kind, O: ReplyShape, Tail> sealed::RowSealed<(Row<K, O>, Tail), K> for Here {}

impl<K: Kind, O: ReplyShape, Tail> RowIndex<(Row<K, O>, Tail), K> for Here {
    type Reply = O;
}

impl<H, Tail, K, I: RowIndex<Tail, K>> sealed::RowSealed<(H, Tail), K> for There<I> {}

impl<H, Tail, K, I: RowIndex<Tail, K>> RowIndex<(H, Tail), K> for There<I> {
    type Reply = I::Reply;
}

/// An actor's declaration lists (ADR-0231 §10): the dependencies it declares
/// in `#[actor(depends(..))]` and the inline children it declares in
/// `#[actor(spawns(..))]`, each as a type-level list `(E1, (E2, (…, ())))` in
/// declaration order.
///
/// `#[actor]` emits the one impl per actor, from the same parsed lists as the
/// dependency records the pre-`init` check reads. Each
/// [`DependsOn<R>`](crate::DependsOn) and [`Spawns<C>`](crate::Spawns) impl
/// names its entry's position in these lists, so none compiles without its
/// entry here. A type that no `#[actor]` expansion built writes this impl
/// itself, as it writes its own dispatch.
pub trait Declared {
    /// The declared dependencies, in `depends(..)` order.
    type Depends;
    /// The declared inline children, in `spawns(..)` order; `()` on native.
    type Spawns;
}
