//! Tuple visitor for inferred view dependencies.

use alloc::boxed::Box;
use core::any::{Any, TypeId, type_name};
use core::error::Error;
use core::marker::PhantomData;

use aether_bloomery_kinds::{Entry, Seq};
use aether_bloomery_view::View;

mod sealed {
    pub trait Sealed {}
}

/// Views a parameter list needs folded together.
///
/// Implemented for each [`View`] that is [`Send`], for [`NoViews`], and for [`And`]. Not
/// implemented for `()` or tuples: those types could grow a [`View`] impl in
/// the view crate, which would overlap a blanket [`View`] impl here. Sealed:
/// implement [`View`] rather than this trait.
pub trait ViewSet: sealed::Sealed {
    /// Immutable references injected into [`crate::Guard::resolve`].
    type Refs<'a>;

    /// Visit each slot in order, including duplicates.
    #[doc(hidden)]
    fn each_view(visit: impl FnMut(ViewCtor));

    /// Bind folded instances in visit order.
    #[doc(hidden)]
    fn refs<'a>(get: impl Fn(TypeId) -> Option<&'a dyn Any>) -> Option<Self::Refs<'a>>;
}

/// Constructor metadata for one view type.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct ViewCtor {
    pub(crate) id: TypeId,
    pub(crate) name: &'static str,
    pub(crate) empty: fn() -> Box<dyn ErasedView>,
}

impl ViewCtor {
    pub(crate) fn of<V: View + Send>() -> Self {
        Self { id: TypeId::of::<V>(), name: type_name::<V>(), empty: || Box::new(Slot { view: V::empty() }) }
    }
}

pub trait ErasedView: Send {
    fn cursor(&self) -> Seq;
    fn as_any(&self) -> &dyn Any;
    fn advance(&mut self, entries: &[Entry]) -> Result<(), Box<dyn Error + 'static>>;
}

struct Slot<V: View> {
    view: V,
}

impl<V: View + Send> ErasedView for Slot<V> {
    fn cursor(&self) -> Seq {
        self.view.cursor()
    }

    fn as_any(&self) -> &dyn Any {
        &self.view
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Box<dyn Error + 'static>> {
        self.view.advance(entries).map_err(|error| Box::new(error) as _)
    }
}

/// Empty view set. Local so it cannot overlap the [`View`] blanket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoViews;

/// Join two [`ViewSet`]s. Local so composition cannot overlap the [`View`] blanket.
pub struct And<A, B>(PhantomData<(A, B)>);

impl sealed::Sealed for NoViews {}

impl ViewSet for NoViews {
    type Refs<'a> = ();

    fn each_view(_visit: impl FnMut(ViewCtor)) {}

    fn refs<'a>(_get: impl Fn(TypeId) -> Option<&'a dyn Any>) -> Option<Self::Refs<'a>> {
        Some(())
    }
}

impl<V: View + Send> sealed::Sealed for V {}

impl<V: View + Send> ViewSet for V {
    type Refs<'a> = &'a V;

    fn each_view(mut visit: impl FnMut(ViewCtor)) {
        visit(ViewCtor::of::<V>());
    }

    fn refs<'a>(get: impl Fn(TypeId) -> Option<&'a dyn Any>) -> Option<Self::Refs<'a>> {
        get(TypeId::of::<V>())?.downcast_ref::<V>()
    }
}

impl<A: ViewSet, B: ViewSet> sealed::Sealed for And<A, B> {}

impl<A: ViewSet, B: ViewSet> ViewSet for And<A, B> {
    type Refs<'a> = (A::Refs<'a>, B::Refs<'a>);

    fn each_view(mut visit: impl FnMut(ViewCtor)) {
        A::each_view(&mut visit);
        B::each_view(visit);
    }

    fn refs<'a>(get: impl Fn(TypeId) -> Option<&'a dyn Any>) -> Option<Self::Refs<'a>> {
        Some((A::refs(&get)?, B::refs(&get)?))
    }
}
