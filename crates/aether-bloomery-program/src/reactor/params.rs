//! Type-level parameter lists a signature macro can emit.
//!
//! Authors write `heads: Heads`, `current: CurrentCompilation`,
//! `cited: Cited`, and `at: At`. The signature macro emits [`Arg`] chains with
//! inferred role markers (`Arg<_, T, Rest>`). Rust selects the view, guard,
//! citations, or position implementation from each concrete parameter type. Distinct role markers
//! keep the impls from overlapping. The macro does not classify type names.

use core::marker::PhantomData;

use crate::view::{At, Cited, Publish};

use crate::reactor::guard::Guard;
use crate::reactor::trigger::Trigger;
use crate::reactor::views::{And, NoViews, ViewSet};

/// Role of a direct published view parameter.
pub enum AsView {}

/// Role of a named guard parameter.
pub enum AsGuard {}

/// Role of the trigger's [`Cited`] parameter: the artifacts the trigger
/// entry cites directly.
pub enum AsCited {}

/// Role of the trigger's [`At`] parameter: the trigger entry's own `seq` and
/// `cause`.
pub enum AsAt {}

/// Empty parameter list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nil;

/// One classified parameter in front of `Rest`.
pub struct Arg<Role, P, Rest = Nil>(PhantomData<(Role, P, Rest)>);

/// Direct view parameter followed by `Rest`.
pub type ViewArg<P, Rest = Nil> = Arg<AsView, P, Rest>;

/// Named guard parameter followed by `Rest`.
pub type GuardArg<P, Rest = Nil> = Arg<AsGuard, P, Rest>;

/// The trigger's [`Cited`] parameter followed by `Rest`.
pub type CitedArg<Rest = Nil> = Arg<AsCited, Cited, Rest>;

/// The trigger's [`At`] parameter followed by `Rest`.
pub type AtArg<Rest = Nil> = Arg<AsAt, At, Rest>;

/// Inferred parameter list. Implemented for [`Nil`] and [`Arg`] chains.
pub trait Params<T: Trigger>: Sized + 'static {
    /// Views this list needs, joined for a single fold pass.
    type Views: ViewSet;

    /// Owned values resolved from the trigger and those views.
    type Value;

    /// Resolve every parameter from the trigger, its entry's own position,
    /// the artifacts its entry cites, and the folded views, or decline if a
    /// named guard returns [`None`].
    fn resolve(trigger: &T, at: At, cited: &Cited, views: <Self::Views as ViewSet>::Refs<'_>) -> Option<Self::Value>;
}

impl<T: Trigger> Params<T> for Nil {
    type Views = NoViews;
    type Value = ();

    fn resolve(_trigger: &T, _at: At, _cited: &Cited, (): ()) -> Option<Self::Value> {
        Some(())
    }
}

impl<T: Trigger, V: Publish + Send, Rest: Params<T>> Params<T> for Arg<AsView, V, Rest> {
    type Views = And<V, Rest::Views>;
    type Value = (V, Rest::Value);

    fn resolve(
        trigger: &T,
        at: At,
        cited: &Cited,
        (view, rest): (<V as ViewSet>::Refs<'_>, <Rest::Views as ViewSet>::Refs<'_>),
    ) -> Option<Self::Value> {
        Some((V::snapshot(view), Rest::resolve(trigger, at, cited, rest)?))
    }
}

impl<T: Trigger, G: Guard<T>, Rest: Params<T>> Params<T> for Arg<AsGuard, G, Rest> {
    type Views = And<G::Views, Rest::Views>;
    type Value = (G, Rest::Value);

    fn resolve(
        trigger: &T,
        at: At,
        cited: &Cited,
        (views, rest): (<G::Views as ViewSet>::Refs<'_>, <Rest::Views as ViewSet>::Refs<'_>),
    ) -> Option<Self::Value> {
        Some((G::resolve(trigger, at, views)?, Rest::resolve(trigger, at, cited, rest)?))
    }
}

/// The trigger's citations, owned: each artifact is a cheap `Blob` clone.
impl<T: Trigger, Rest: Params<T>> Params<T> for Arg<AsCited, Cited, Rest> {
    type Views = Rest::Views;
    type Value = (Cited, Rest::Value);

    fn resolve(trigger: &T, at: At, cited: &Cited, rest: <Rest::Views as ViewSet>::Refs<'_>) -> Option<Self::Value> {
        Some((cited.clone(), Rest::resolve(trigger, at, cited, rest)?))
    }
}

/// The trigger entry's own position, copied.
impl<T: Trigger, Rest: Params<T>> Params<T> for Arg<AsAt, At, Rest> {
    type Views = Rest::Views;
    type Value = (At, Rest::Value);

    fn resolve(trigger: &T, at: At, cited: &Cited, rest: <Rest::Views as ViewSet>::Refs<'_>) -> Option<Self::Value> {
        Some((at, Rest::resolve(trigger, at, cited, rest)?))
    }
}
