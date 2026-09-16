//! Tuple selections of one through eight views.

use std::any::{Any, TypeId, type_name};

use crate::view::{ErasedView, Slot, View};

mod sealed {
    pub trait Sealed {}
}

/// A tuple of views the registry can inject together.
///
/// Implemented for each [`View`] and for tuples of one through eight views.
/// Duplicate types share one cached instance. Sealed: implement [`View`]
/// rather than this trait.
pub trait ViewSelection: sealed::Sealed {
    /// Immutable references injected into [`crate::Views::with`].
    type Refs<'a>;

    /// Visit each tuple slot in order, including duplicates.
    #[doc(hidden)]
    fn each_view(visit: impl FnMut(ViewCtor));

    /// Bind cached instances in tuple order.
    #[doc(hidden)]
    fn refs<'a>(get: impl Fn(TypeId) -> Option<&'a dyn Any>) -> Option<Self::Refs<'a>>;
}

/// Constructor metadata for one native view type.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct ViewCtor {
    pub(crate) id: TypeId,
    pub(crate) name: &'static str,
    pub(crate) empty: fn() -> Box<dyn ErasedView>,
}

impl ViewCtor {
    pub(crate) fn of<V: View>() -> Self {
        Self { id: TypeId::of::<V>(), name: type_name::<V>(), empty: || Box::new(Slot { view: V::empty() }) }
    }
}

impl<V: View> sealed::Sealed for V {}

impl<V: View> ViewSelection for V {
    type Refs<'a> = &'a V;

    fn each_view(mut visit: impl FnMut(ViewCtor)) {
        visit(ViewCtor::of::<V>());
    }

    fn refs<'a>(get: impl Fn(TypeId) -> Option<&'a dyn Any>) -> Option<Self::Refs<'a>> {
        get(TypeId::of::<V>())?.downcast_ref::<V>()
    }
}

macro_rules! impl_view_selection {
    ($($name:ident),+) => {
        impl<$($name: View),+> sealed::Sealed for ($($name,)+) {}

        impl<$($name: View),+> ViewSelection for ($($name,)+) {
            type Refs<'a> = ($(&'a $name,)+);

            fn each_view(mut visit: impl FnMut(ViewCtor)) {
                $(visit(ViewCtor::of::<$name>());)+
            }

            fn refs<'a>(get: impl Fn(TypeId) -> Option<&'a dyn Any>) -> Option<Self::Refs<'a>> {
                Some((
                    $(
                        get(TypeId::of::<$name>())?.downcast_ref::<$name>()?,
                    )+
                ))
            }
        }
    };
}

impl_view_selection!(V1);
impl_view_selection!(V1, V2);
impl_view_selection!(V1, V2, V3);
impl_view_selection!(V1, V2, V3, V4);
impl_view_selection!(V1, V2, V3, V4, V5);
impl_view_selection!(V1, V2, V3, V4, V5, V6);
impl_view_selection!(V1, V2, V3, V4, V5, V6, V7);
impl_view_selection!(V1, V2, V3, V4, V5, V6, V7, V8);
