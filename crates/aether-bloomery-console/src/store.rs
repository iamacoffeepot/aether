//! Per-resource cache. Screens see this read-only.

use std::fmt::Display;
use std::time::{Duration, Instant};

use crate::dto::ViewDocument;

/// Which coordinator resource a screen can subscribe to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    View,
}

/// Which fetch thread serves a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Live,
    Bulk,
}

impl ResourceKey {
    #[must_use]
    pub fn lane(self) -> Lane {
        match self {
            Self::View => Lane::Live,
        }
    }

    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::View => "/view",
        }
    }
}

/// Last sample of one resource. An error keeps `value` and dims the paint.
#[derive(Clone, Debug)]
pub struct Cell<T> {
    pub value: Option<T>,
    pub fetched_at: Option<Instant>,
    pub inflight: bool,
    pub error: Option<String>,
    completed_at: Option<Instant>,
}

impl<T> Default for Cell<T> {
    fn default() -> Self {
        Self { value: None, fetched_at: None, inflight: false, error: None, completed_at: None }
    }
}

impl<T> Cell<T> {
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.error.is_some()
    }

    #[must_use]
    pub fn sample_age(&self) -> Option<Duration> {
        self.fetched_at.map(|when| when.elapsed())
    }

    pub fn apply_ok(&mut self, value: T) {
        self.value = Some(value);
        self.fetched_at = Some(Instant::now());
        self.completed_at = self.fetched_at;
        self.inflight = false;
        self.error = None;
    }

    pub fn apply_err(&mut self, error: impl Display) {
        self.inflight = false;
        self.error = Some(error.to_string());
        self.completed_at = Some(Instant::now());
    }
}

/// Shell-owned resource cache. Cadence lives here, not on a screen.
#[derive(Debug)]
pub struct Store {
    view: Cell<ViewDocument>,
    view_cadence: Duration,
}

impl Store {
    #[must_use]
    pub fn new(view_cadence: Duration) -> Self {
        Self { view: Cell::default(), view_cadence }
    }

    #[must_use]
    pub fn view(&self) -> &Cell<ViewDocument> {
        &self.view
    }

    #[must_use]
    pub fn cadence(&self, key: ResourceKey) -> Duration {
        match key {
            ResourceKey::View => self.view_cadence,
        }
    }

    #[must_use]
    pub fn due(&self, key: ResourceKey) -> bool {
        let cell = self.cell(key);
        if cell.inflight {
            return false;
        }
        match cell.completed_at {
            None => true,
            Some(at) => at.elapsed() >= self.cadence(key),
        }
    }

    #[must_use]
    pub fn is_inflight(&self, key: ResourceKey) -> bool {
        self.cell(key).inflight
    }

    pub fn mark_inflight(&mut self, key: ResourceKey) {
        match key {
            ResourceKey::View => self.view.inflight = true,
        }
    }

    pub fn apply_view(&mut self, result: Result<ViewDocument, String>) {
        match result {
            Ok(view) => self.view.apply_ok(view),
            Err(error) => self.view.apply_err(error),
        }
    }

    pub fn apply_err(&mut self, key: ResourceKey, error: impl Display) {
        match key {
            ResourceKey::View => self.view.apply_err(error),
        }
    }

    fn cell(&self, key: ResourceKey) -> &Cell<ViewDocument> {
        match key {
            ResourceKey::View => &self.view,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Store;
    use crate::dto::{BloomView, DigestHex, MemberView, ViewDocument};
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    #[test]
    fn a_failed_poll_keeps_the_last_board() {
        // The plausible bug: a connection-refused mid-run clears the rows,
        // so a coordinator restart blanks the board instead of dimming it.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView { workpiece: "wp-a".to_owned(), ..MemberView::default() }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_view(Ok(view));
        assert_eq!(store.view().value.as_ref().map(|view| view.blooms.len()), Some(1));
        assert!(!store.view().is_stale());
        store.apply_view(Err("connection refused".to_owned()));
        assert!(store.view().is_stale());
        assert_eq!(store.view().value.as_ref().map(|view| view.blooms.len()), Some(1));
        assert_eq!(store.view().error.as_deref(), Some("connection refused"));
        assert!(store.view().fetched_at.is_some());
    }
}
