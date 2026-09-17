//! Named guard resolved from a trigger and its inferred views.

use crate::trigger::Trigger;
use crate::views::{NoViews, ViewSet};

/// Named dependency of a trigger. [`None`] declines invocation.
///
/// `Views` is walked by [`ViewSet`]; authors do not register views or wrap
/// parameters. Duplicate view types in a tuple share one folded instance.
pub trait Guard<T: Trigger>: Sized + 'static {
    /// Views this guard reads. Tuples are allowed; each concrete type folds once.
    type Views: ViewSet;

    /// Resolve this guard, or decline.
    fn resolve(trigger: &T, views: <Self::Views as ViewSet>::Refs<'_>) -> Option<Self>;
}

impl<T: Trigger> Guard<T> for () {
    type Views = NoViews;

    fn resolve(_trigger: &T, (): ()) -> Option<Self> {
        Some(())
    }
}
