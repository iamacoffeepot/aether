//! Per-resource cache. Screens see this read-only.

use std::collections::HashMap;
use std::fmt::Display;
use std::time::{Duration, Instant};

use crate::dto::{DecodedArtifact, DigestHex, DispatchFilePage, JournalPage, JournalRecordView, ViewDocument};

/// Which coordinator resource a screen can subscribe to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    View,
    Journal(JournalQuery),
    Artifact(DigestHex),
    Transcript(TranscriptQuery),
}

/// Query identity for one journal page. Filter text lives on the screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct JournalQuery {
    pub bloom: Option<DigestHex>,
    pub from_sequence: Option<u64>,
}

impl JournalQuery {
    #[must_use]
    pub fn path(self) -> String {
        let mut parts = Vec::new();
        if let Some(bloom) = self.bloom {
            parts.push(format!("bloom={}", bloom.as_hex()));
        }
        if let Some(from) = self.from_sequence {
            parts.push(format!("from_sequence={from}"));
        }
        if parts.is_empty() {
            "/journal".to_owned()
        } else {
            format!("/journal?{}", parts.join("&"))
        }
    }
}

/// One ranged transcript page. `live` is cadence only — it is not on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TranscriptQuery {
    pub nonce: String,
    pub cursor: Option<u64>,
    pub live: bool,
}

impl TranscriptQuery {
    #[must_use]
    pub fn path(&self) -> String {
        self.cursor.map_or_else(
            || format!("/dispatches/{}/transcript", self.nonce),
            |cursor| format!("/dispatches/{}/transcript?cursor={cursor}", self.nonce),
        )
    }
}

/// Which fetch thread serves a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Live,
    Bulk,
}

impl ResourceKey {
    #[must_use]
    pub fn lane(&self) -> Lane {
        match self {
            Self::View => Lane::Live,
            Self::Journal(_) | Self::Artifact(_) | Self::Transcript(_) => Lane::Bulk,
        }
    }

    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::View => "/view".to_owned(),
            Self::Journal(query) => query.path(),
            Self::Artifact(digest) => format!("/artifacts/{}/decoded", digest.as_hex()),
            Self::Transcript(query) => query.path(),
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

    fn on_demand_due(&self) -> bool {
        !self.inflight && self.completed_at.is_none()
    }
}

/// Shell-owned resource cache. Cadence lives here, not on a screen.
#[derive(Debug)]
pub struct Store {
    view: Cell<ViewDocument>,
    view_cadence: Duration,
    journals: HashMap<JournalQuery, Cell<JournalPage>>,
    artifacts: HashMap<DigestHex, Cell<DecodedArtifact>>,
    transcripts: HashMap<TranscriptQuery, Cell<DispatchFilePage>>,
}

impl Store {
    #[must_use]
    pub fn new(view_cadence: Duration) -> Self {
        Self {
            view: Cell::default(),
            view_cadence,
            journals: HashMap::new(),
            artifacts: HashMap::new(),
            transcripts: HashMap::new(),
        }
    }

    #[must_use]
    pub fn view(&self) -> &Cell<ViewDocument> {
        &self.view
    }

    #[must_use]
    pub fn journal(&self, query: JournalQuery) -> Option<&Cell<JournalPage>> {
        self.journals.get(&query)
    }

    #[must_use]
    pub fn artifact(&self, digest: DigestHex) -> Option<&Cell<DecodedArtifact>> {
        self.artifacts.get(&digest)
    }

    #[must_use]
    pub fn transcript(&self, query: &TranscriptQuery) -> Option<&Cell<DispatchFilePage>> {
        self.transcripts.get(query)
    }

    #[must_use]
    pub fn record(&self, sequence: u64) -> Option<&JournalRecordView> {
        self.journals
            .values()
            .filter_map(|cell| cell.value.as_ref())
            .find_map(|page| page.records.iter().find(|record| record.sequence == sequence))
    }

    #[must_use]
    pub fn cadence(&self, key: &ResourceKey) -> Duration {
        match key {
            ResourceKey::View => self.view_cadence,
            ResourceKey::Transcript(query) if query.live => self.view_cadence,
            ResourceKey::Journal(_) | ResourceKey::Artifact(_) | ResourceKey::Transcript(_) => Duration::ZERO,
        }
    }

    #[must_use]
    pub fn due(&self, key: &ResourceKey) -> bool {
        match key {
            ResourceKey::View => {
                if self.view.inflight {
                    return false;
                }
                self.view.completed_at.is_none_or(|at| at.elapsed() >= self.view_cadence)
            }
            ResourceKey::Journal(query) => self.journals.get(query).is_none_or(Cell::on_demand_due),
            ResourceKey::Artifact(digest) => self.artifacts.get(digest).is_none_or(Cell::on_demand_due),
            ResourceKey::Transcript(query) if query.live => {
                let Some(cell) = self.transcripts.get(query) else {
                    return true;
                };
                !cell.inflight && cell.completed_at.is_none_or(|at| at.elapsed() >= self.view_cadence)
            }
            ResourceKey::Transcript(query) => self.transcripts.get(query).is_none_or(Cell::on_demand_due),
        }
    }

    #[must_use]
    pub fn is_inflight(&self, key: &ResourceKey) -> bool {
        match key {
            ResourceKey::View => self.view.inflight,
            ResourceKey::Journal(query) => self.journals.get(query).is_some_and(|cell| cell.inflight),
            ResourceKey::Artifact(digest) => self.artifacts.get(digest).is_some_and(|cell| cell.inflight),
            ResourceKey::Transcript(query) => self.transcripts.get(query).is_some_and(|cell| cell.inflight),
        }
    }

    pub fn mark_inflight(&mut self, key: &ResourceKey) {
        match key {
            ResourceKey::View => self.view.inflight = true,
            ResourceKey::Journal(query) => self.journals.entry(*query).or_default().inflight = true,
            ResourceKey::Artifact(digest) => self.artifacts.entry(*digest).or_default().inflight = true,
            ResourceKey::Transcript(query) => self.transcripts.entry(query.clone()).or_default().inflight = true,
        }
    }

    pub fn apply_view(&mut self, result: Result<ViewDocument, String>) {
        match result {
            Ok(view) => self.view.apply_ok(view),
            Err(error) => self.view.apply_err(error),
        }
    }

    pub fn apply_journal(&mut self, query: JournalQuery, result: Result<JournalPage, String>) {
        let cell = self.journals.entry(query).or_default();
        match result {
            Ok(page) => cell.apply_ok(page),
            Err(error) => cell.apply_err(error),
        }
    }

    pub fn apply_artifact(&mut self, digest: DigestHex, result: Result<DecodedArtifact, String>) {
        let cell = self.artifacts.entry(digest).or_default();
        match result {
            Ok(body) => cell.apply_ok(body),
            Err(error) => cell.apply_err(error),
        }
    }

    pub fn apply_transcript(&mut self, query: TranscriptQuery, result: Result<DispatchFilePage, String>) {
        let cell = self.transcripts.entry(query).or_default();
        match result {
            Ok(page) => cell.apply_ok(page),
            Err(error) => cell.apply_err(error),
        }
    }

    pub fn apply_err(&mut self, key: &ResourceKey, error: impl Display) {
        match key {
            ResourceKey::View => self.view.apply_err(error),
            ResourceKey::Journal(query) => self.journals.entry(*query).or_default().apply_err(error),
            ResourceKey::Artifact(digest) => self.artifacts.entry(*digest).or_default().apply_err(error),
            ResourceKey::Transcript(query) => self.transcripts.entry(query.clone()).or_default().apply_err(error),
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
