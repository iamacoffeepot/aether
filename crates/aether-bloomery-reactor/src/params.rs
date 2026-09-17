//! Type-level parameter lists a signature macro can emit.
//!
//! Authors write `heads: Heads` and `current: CurrentCompilation`. The macro
//! emits [`Arg`] chains with inferred role markers. Rust selects the view or
//! guard implementation from each concrete parameter type. Distinct role
//! markers keep the impls from overlapping.

use core::marker::PhantomData;

use aether_bloomery_view::Publish;

use crate::guard::Guard;
use crate::trigger::Trigger;
use crate::views::{And, NoViews, ViewSet};

/// Role of a direct published view parameter.
pub enum AsView {}

/// Role of a named guard parameter.
pub enum AsGuard {}

/// Empty parameter list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nil;

/// One classified parameter in front of `Rest`.
pub struct Arg<Role, P, Rest = Nil>(PhantomData<(Role, P, Rest)>);

/// Direct view parameter followed by `Rest`.
pub type ViewArg<P, Rest = Nil> = Arg<AsView, P, Rest>;

/// Named guard parameter followed by `Rest`.
pub type GuardArg<P, Rest = Nil> = Arg<AsGuard, P, Rest>;

/// Inferred parameter list. Implemented for [`Nil`] and [`Arg`] chains.
pub trait Params<T: Trigger>: Sized + 'static {
    /// Views this list needs, joined for a single fold pass.
    type Views: ViewSet;

    /// Owned values resolved from the trigger and those views.
    type Value;

    /// Resolve every parameter, or decline if a named guard returns [`None`].
    fn resolve(trigger: &T, views: <Self::Views as ViewSet>::Refs<'_>) -> Option<Self::Value>;
}

impl<T: Trigger> Params<T> for Nil {
    type Views = NoViews;
    type Value = ();

    fn resolve(_trigger: &T, (): ()) -> Option<Self::Value> {
        Some(())
    }
}

impl<T: Trigger, V: Publish, Rest: Params<T>> Params<T> for Arg<AsView, V, Rest> {
    type Views = And<V, Rest::Views>;
    type Value = (V, Rest::Value);

    fn resolve(
        trigger: &T,
        (view, rest): (<V as ViewSet>::Refs<'_>, <Rest::Views as ViewSet>::Refs<'_>),
    ) -> Option<Self::Value> {
        Some((V::snapshot(view), Rest::resolve(trigger, rest)?))
    }
}

impl<T: Trigger, G: Guard<T>, Rest: Params<T>> Params<T> for Arg<AsGuard, G, Rest> {
    type Views = And<G::Views, Rest::Views>;
    type Value = (G, Rest::Value);

    fn resolve(
        trigger: &T,
        (views, rest): (<G::Views as ViewSet>::Refs<'_>, <Rest::Views as ViewSet>::Refs<'_>),
    ) -> Option<Self::Value> {
        Some((G::resolve(trigger, views)?, Rest::resolve(trigger, rest)?))
    }
}
