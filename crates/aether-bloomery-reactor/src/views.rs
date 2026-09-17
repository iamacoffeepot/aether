//! Tuple visitor for inferred view dependencies.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::any::{Any, TypeId, type_name};
use core::error::Error;
use core::marker::PhantomData;

use aether_bloomery_kinds::{Entry, Seq};
use aether_bloomery_view::{Heads, Publish, View};
use aether_data::Kind;

use crate::error::PrepareError;

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

/// View that can travel between the views owner and reactor peers.
///
/// [`Self::NAME`] is the portable snapshot identity. It is never a Rust
/// [`TypeId`]. Implement this for each view a `#[reactor]` arm or named guard
/// folds; [`Publish`] supplies the snapshot codec.
pub trait BundledView: Publish + Send {
    /// Stable published name used in [`crate::PublishedView`].
    const NAME: &'static str;
}

impl BundledView for Heads {
    const NAME: &'static str = <Self as Kind>::NAME;
}

type DecodePublished = fn(&[u8]) -> Result<Box<dyn ErasedView>, PrepareError>;

/// Constructor metadata for one bundled published view.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct PublishCtor {
    pub(crate) id: TypeId,
    pub(crate) name: &'static str,
    pub(crate) encode: fn(&dyn Any) -> Result<Vec<u8>, PrepareError>,
    pub(crate) decode: DecodePublished,
}

impl PublishCtor {
    pub(crate) fn of<V: BundledView>() -> Self {
        Self {
            id: TypeId::of::<V>(),
            name: V::NAME,
            encode: |any| {
                let view = any
                    .downcast_ref::<V>()
                    .ok_or(PrepareError::Poisoned { view: V::NAME, last_trusted_cursor: Seq(0) })?;
                V::encode(view).map_err(|error| PrepareError::Snapshot { view: V::NAME, source: Box::new(error) })
            },
            decode: |bytes| {
                let view = V::decode(bytes)
                    .map_err(|error| PrepareError::Snapshot { view: V::NAME, source: Box::new(error) })?;
                Ok(box_view(view))
            },
        }
    }
}

pub trait ErasedView: Send {
    fn cursor(&self) -> Seq;
    fn as_any(&self) -> &dyn Any;
    fn advance(&mut self, entries: &[Entry]) -> Result<(), Box<dyn Error + 'static>>;
}

pub fn box_view<V: View + Send>(view: V) -> Box<dyn ErasedView> {
    Box::new(Slot { view })
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

/// Views a reactor parameter list can snapshot and reconstruct over mail.
///
/// Implemented for [`BundledView`], [`NoViews`], and [`And`]. A `#[reactor]`
/// arm whose inferred views are not publishable fails this bound.
pub trait PublishSet: ViewSet {
    /// Visit each published slot in order, including duplicates.
    fn each_published(visit: impl FnMut(PublishCtor));
}

impl PublishSet for NoViews {
    fn each_published(_visit: impl FnMut(PublishCtor)) {}
}

impl<V: BundledView> PublishSet for V {
    fn each_published(mut visit: impl FnMut(PublishCtor)) {
        visit(PublishCtor::of::<V>());
    }
}

impl<A: PublishSet, B: PublishSet> PublishSet for And<A, B> {
    fn each_published(mut visit: impl FnMut(PublishCtor)) {
        A::each_published(&mut visit);
        B::each_published(visit);
    }
}
