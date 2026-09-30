//! Named guard resolved from a trigger and its inferred views.

use aether_bloomery_view::At;

use crate::trigger::Trigger;
use crate::views::{NoViews, ViewSet};

/// Named dependency of a trigger. [`None`] declines invocation.
///
/// `Views` is walked by [`ViewSet`]; authors do not register views or wrap
/// parameters. Duplicate view types in a tuple share one folded instance.
pub trait Guard<T: Trigger>: Sized + 'static {
    /// Views this guard reads. Tuples are allowed; each concrete type folds once.
    type Views: ViewSet;

    /// Resolve this guard from the trigger, the trigger entry's own [`At`],
    /// and the folded views, or decline.
    fn resolve(trigger: &T, at: At, views: <Self::Views as ViewSet>::Refs<'_>) -> Option<Self>;
}

impl<T: Trigger> Guard<T> for () {
    type Views = NoViews;

    fn resolve(_trigger: &T, _at: At, (): ()) -> Option<Self> {
        Some(())
    }
}
