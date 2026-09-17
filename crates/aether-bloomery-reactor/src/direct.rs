//! Direct view parameters. [`aether_bloomery_view::Heads`] is the owned case.

use aether_bloomery_view::Publish;

use crate::views::ViewSet;

/// A parameter that is itself a folded view, not a named guard.
///
/// Implemented for [`Publish`] views that are [`Send`]. Signature authoring uses the view type
/// directly (`heads: Heads`) without a wrapper. Absence of a direct view is
/// [`crate::Nil`] / [`crate::NoViews`] on the inferred list, not `()`.
pub trait Direct: Publish + Send {
    /// Views this parameter needs. For a published view this is `Self`.
    type Views: ViewSet;

    /// Take an owned parameter from the folded views.
    fn from_views(views: <Self::Views as ViewSet>::Refs<'_>) -> Self;
}

impl<V: Publish + Send> Direct for V {
    type Views = V;

    fn from_views(view: &V) -> Self {
        V::snapshot(view)
    }
}
