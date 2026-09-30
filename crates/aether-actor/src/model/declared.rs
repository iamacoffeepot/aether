//! Declaration lists and their positions (ADR-0231 §10): the one place an
//! actor's contract rows, declared dependencies, declared inline children,
//! and declared parents are stated, and the sealed index each per-entry
//! marker names into them.
//!
//! Coherence allows one impl of a trait for a type. `#[actor]` emits exactly
//! one [`Contracts`](crate::Contracts) impl and one [`Declared`] impl per
//! actor, and `export!` one `ListedModule` impl per module, each carrying its
//! entries as a type-level list `(E1, (E2, (…, ())))`. Every per-entry marker
//! ([`Contract<K>`](crate::Contract), [`DependsOn<R>`](crate::DependsOn),
//! [`Spawns<C>`](crate::Spawns), [`ChildOf<P>`](crate::ChildOf),
//! [`Rebuildable<M>`](crate::Rebuildable))
//! names the position of its entry in that list as `type Index`, and the
//! marker's `Index` bound holds only when that position holds that entry. A
//! hand-written marker either repeats an impl the expansion emitted (`E0119`)
//! or names a position that holds a different entry or none (`E0277`), so a
//! marker that compiles is backed by a declaration.
//!
//! The checks read the same lists: the pre-`init` dependency check on both
//! transports walks [`Declared::Depends`] as a [`DependencyList`], and
//! `export!`'s inline-child coverage check bounds [`Declared::Spawns`]. So a
//! hand-written [`Declared`] impl is checked exactly as an emitted one is.
//!
//! A position is [`Here`] (the list's head) or [`There<I>`] (position `I` of
//! the tail). The expansion writes each entry's position; no one else needs
//! to. A contract row list keeps every handler's slot whatever the enabled
//! features: a `#[cfg]`-gated handler whose predicates are off fills its slot
//! with [`Gap`], which only [`There`] steps over, so every other handler's
//! position is the same in every configuration.

use core::iter;
use core::marker::PhantomData;

use aether_data::__derive_runtime::canonical::{inputs_dependency_len, write_inputs_dependency};
use aether_data::{INPUTS_SECTION_VERSION, Kind};

use super::contract::ReplyShape;
use super::protocol::Row;
use super::{CallerAddressable, DependencyResolver, HandlesKind, Singleton};

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

    /// Private supertrait sealing [`super::DependencyList`] to its two
    /// structural impls.
    pub trait DependencySealed {}

    /// Private supertrait sealing [`super::AllHandle`] to its two structural
    /// impls.
    pub trait AllHandleSealed<K> {}
}

/// `I: ListIndex<L, T>` holds when position `I` of the declaration list `L`
/// holds `T`. Sealed, and implemented only structurally: [`Here`] for a list
/// whose head is `T`, and [`There<I>`] for a list whose tail holds `T` at `I`.
#[diagnostic::on_unimplemented(
    message = "the position `{Self}` does not hold `{T}` in the declared list `{L}`",
    label = "no declaration backs this entry",
    note = "a dependency is declared in `depends(..)`, an inline child in `spawns(..)`, and a parent in \
            `child_of(..)` on the actor's `#[actor(..)]`, and a module's listing in its `export!`; only those \
            expansions write these markers"
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

/// `L: AllHandle<K>` holds when every entry of the non-empty declared parent
/// list `L` handles `K`. It is the bound a send through
/// [`InlineParent`](crate::InlineParent) carries, because a child that lists
/// several parents in `child_of(..)` does not know which of them holds it, so
/// a kind it sends up must be one every parent handles. With one parent it
/// reduces to `P: HandlesKind<K>`.
///
/// Sealed, and implemented only structurally: `(P, ())` when `P` handles `K`,
/// and `(P, (Q, Tail))` when `P` handles `K` and `(Q, Tail)` does.
#[diagnostic::on_unimplemented(
    message = "a declared parent in `{Self}` has no handler for `{K}`",
    note = "every type in `child_of(..)` must handle a kind sent through `ctx.parent()`"
)]
pub trait AllHandle<K: Kind>: sealed::AllHandleSealed<K> {}

impl<K: Kind, P: HandlesKind<K>> sealed::AllHandleSealed<K> for (P, ()) {}

impl<K: Kind, P: HandlesKind<K>> AllHandle<K> for (P, ()) {}

impl<K: Kind, P: HandlesKind<K>, Q, Tail> sealed::AllHandleSealed<K> for (P, (Q, Tail)) where (Q, Tail): AllHandle<K> {}

impl<K: Kind, P: HandlesKind<K>, Q, Tail> AllHandle<K> for (P, (Q, Tail)) where (Q, Tail): AllHandle<K> {}

/// An actor's declaration lists (ADR-0231 §10): the dependencies it declares
/// in `#[actor(depends(..))]`, the inline children it declares in
/// `#[actor(spawns(..))]`, and the parents it declares in
/// `#[actor(child_of(..))]`, each as a type-level list `(E1, (E2, (…, ())))`
/// in declaration order.
///
/// These lists are what the checks read. The native birth check reads
/// [`Depends`](Declared::Depends) through [`declared_dependencies`], and
/// `export!` writes a guest's dependency records from it, so the pre-`init`
/// check on both transports refuses the birth while a listed dependency is
/// not `Live`. `export!`'s inline-child coverage check reads
/// [`Spawns`](Declared::Spawns), and each spawner's
/// [`Spawns<C>`](crate::Spawns) impl names its own position in `C`'s
/// [`Parents`](Declared::Parents). Whatever an impl lists is exactly what is
/// checked.
///
/// `#[actor]` emits the one impl per actor. Each
/// [`DependsOn<R>`](crate::DependsOn), [`Spawns<C>`](crate::Spawns), and
/// [`ChildOf<P>`](crate::ChildOf) impl names its entry's position in these
/// lists, so none compiles without its entry here. A type that no `#[actor]`
/// expansion built writes this impl itself, as it writes its own dispatch; `NativeActor` and
/// [`WasmActor`](crate::WasmActor) both require it.
pub trait Declared {
    /// The declared dependencies, in `depends(..)` order.
    type Depends: DependencyList;
    /// The declared inline children, in `spawns(..)` order; `()` on native.
    type Spawns;
    /// The declared parents, in `child_of(..)` order; `()` for a root-only
    /// actor.
    type Parents;
}

/// A declared dependency list `(R1, (R2, (…, ())))` (ADR-0231 §10): each
/// entry a keyless actor with a [`DependencyResolver`] strategy, the same
/// bound [`DependsOn<R>`](crate::DependsOn) carries.
///
/// Sealed, and implemented only structurally: `()` is the empty list, and
/// `(R, Tail)` holds when `R` is declarable and `Tail` is a list. Its one
/// item is the list's first [`DependencyLink`], which the pre-`init` checks
/// walk.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a declared dependency list",
    note = "a dependency list is `()` or `(R, Tail)`, where `R` is a root singleton with a `One` resolver"
)]
pub trait DependencyList: sealed::DependencySealed {
    /// The list's first entry, or `None` for the empty list.
    #[doc(hidden)]
    const FIRST: Option<&'static DependencyLink>;
}

impl sealed::DependencySealed for () {}

impl DependencyList for () {
    const FIRST: Option<&'static DependencyLink> = None;
}

impl<R, Tail> sealed::DependencySealed for (R, Tail)
where
    R: Singleton + CallerAddressable,
    R::Resolver: DependencyResolver,
    Tail: DependencyList,
{
}

impl<R, Tail> DependencyList for (R, Tail)
where
    R: Singleton + CallerAddressable,
    R::Resolver: DependencyResolver,
    Tail: DependencyList,
{
    const FIRST: Option<&'static DependencyLink> = Some(&DependencyLink {
        resolver: <R::Resolver as DependencyResolver>::TAG,
        namespace: R::NAMESPACE,
        next: Tail::FIRST,
    });
}

/// One entry of a [`DependencyList`]: the dependency's resolver tag and
/// namespace, then the next entry. Built only by the list's `(R, Tail)` impl,
/// so every link names a declarable actor.
#[derive(Debug)]
pub struct DependencyLink {
    resolver: u8,
    namespace: &'static str,
    next: Option<&'static Self>,
}

/// `A`'s declared dependencies as `(resolver tag, namespace)` pairs, in
/// `depends(..)` order: the list the native birth check reads.
pub fn declared_dependencies<A: Declared>() -> impl Iterator<Item = (u8, &'static str)> {
    iter::successors(<A::Depends as DependencyList>::FIRST, |link| link.next)
        .map(|link| (link.resolver, link.namespace))
}

/// The byte length of the version-framed `Dependency` records
/// [`write_dependency_records`] writes for the list starting at `first`.
#[doc(hidden)]
#[must_use]
pub const fn dependency_records_len(first: Option<&'static DependencyLink>) -> usize {
    let mut len = 0;
    let mut next = first;
    while let Some(link) = next {
        len += 1 + inputs_dependency_len(link.resolver, link.namespace);
        next = link.next;
    }
    len
}

/// Write one version-framed `Dependency` record per entry of the list
/// starting at `first` into `out` at `cursor`, in list order, returning the
/// new cursor. `export!` writes a guest's dependency records with it, so the
/// host's pre-`init` check reads the same list the native birth check does.
#[doc(hidden)]
#[must_use]
pub const fn write_dependency_records(
    first: Option<&'static DependencyLink>,
    out: &mut [u8],
    mut cursor: usize,
) -> usize {
    let mut next = first;
    while let Some(link) = next {
        out[cursor] = INPUTS_SECTION_VERSION;
        cursor = write_inputs_dependency(link.resolver, link.namespace, out, cursor + 1);
        next = link.next;
    }
    cursor
}
