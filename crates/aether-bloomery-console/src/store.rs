//! Per-resource cache. Screens see this read-only.

use std::collections::HashMap;
use std::fmt::Display;
use std::time::{Duration, Instant};

use crate::dto::{
    CommissionShowView, CommissionsView, DecodedArtifact, DigestHex, DispatchFilePage, JournalPage, JournalRecordView,
    MetricDay, MetricDispatch, MetricsSeat, MetricsSummary, MetricsTimeline, SpendWindowView, ViewDocument,
};

/// Which coordinator resource a screen can subscribe to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    View,
    Journal(JournalQuery),
    Artifact(DigestHex),
    Transcript(TranscriptQuery),
    MetricsSummary,
    MetricsDays,
    MetricsTimeline(DigestHex),
    MetricsSeats,
    MetricsDispatches,
    Spend,
    Commissions,
    Commission(String),
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
            Self::Journal(_)
            | Self::Artifact(_)
            | Self::Transcript(_)
            | Self::MetricsSummary
            | Self::MetricsDays
            | Self::MetricsTimeline(_)
            | Self::MetricsSeats
            | Self::MetricsDispatches
            | Self::Spend
            | Self::Commissions
            | Self::Commission(_) => Lane::Bulk,
        }
    }

    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::View => "/view".to_owned(),
            Self::Journal(query) => query.path(),
            Self::Artifact(digest) => format!("/artifacts/{}/decoded", digest.as_hex()),
            Self::Transcript(query) => query.path(),
            Self::MetricsSummary => "/metrics/summary".to_owned(),
            Self::MetricsDays => "/metrics/days".to_owned(),
            Self::MetricsTimeline(bloom) => format!("/metrics/blooms/{}/timeline", bloom.as_hex()),
            Self::MetricsSeats => "/metrics/seats".to_owned(),
            Self::MetricsDispatches => "/metrics/dispatches".to_owned(),
            Self::Spend => "/spend".to_owned(),
            Self::Commissions => "/commissions".to_owned(),
            Self::Commission(id) => format!("/commissions/{}", path_segment(id)),
        }
    }
}

fn path_segment(id: &str) -> String {
    let mut out = String::new();
    for byte in id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Whether this connection's coordinator serves the commission read routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommissionCapability {
    /// `GET /commissions` returned 404. Cached for the connection.
    Absent,
    /// `GET /commissions` returned a document.
    Present,
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

    fn settle(&mut self) {
        self.inflight = false;
        self.error = None;
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
    summary: Cell<MetricsSummary>,
    days: Cell<Vec<MetricDay>>,
    timelines: HashMap<DigestHex, Cell<MetricsTimeline>>,
    seats: Cell<Vec<MetricsSeat>>,
    dispatches: Cell<Vec<MetricDispatch>>,
    spend: Cell<SpendWindowView>,
    commission_capability: Option<CommissionCapability>,
    commissions: Cell<CommissionsView>,
    commission_shows: HashMap<String, Cell<CommissionShowView>>,
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
            summary: Cell::default(),
            days: Cell::default(),
            timelines: HashMap::new(),
            seats: Cell::default(),
            dispatches: Cell::default(),
            spend: Cell::default(),
            commission_capability: None,
            commissions: Cell::default(),
            commission_shows: HashMap::new(),
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
    pub fn summary(&self) -> &Cell<MetricsSummary> {
        &self.summary
    }

    #[must_use]
    pub fn days(&self) -> &Cell<Vec<MetricDay>> {
        &self.days
    }

    #[must_use]
    pub fn timeline(&self, bloom: DigestHex) -> Option<&Cell<MetricsTimeline>> {
        self.timelines.get(&bloom)
    }

    #[must_use]
    pub fn seats(&self) -> &Cell<Vec<MetricsSeat>> {
        &self.seats
    }

    #[must_use]
    pub fn dispatches(&self) -> &Cell<Vec<MetricDispatch>> {
        &self.dispatches
    }

    #[must_use]
    pub fn spend(&self) -> &Cell<SpendWindowView> {
        &self.spend
    }

    #[must_use]
    pub fn commission_capability(&self) -> Option<CommissionCapability> {
        self.commission_capability
    }

    #[must_use]
    pub fn commissions(&self) -> &Cell<CommissionsView> {
        &self.commissions
    }

    #[must_use]
    pub fn commission(&self, id: &str) -> Option<&Cell<CommissionShowView>> {
        self.commission_shows.get(id)
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
            ResourceKey::View | ResourceKey::MetricsSummary | ResourceKey::MetricsDays | ResourceKey::Spend => {
                self.view_cadence
            }
            ResourceKey::Transcript(query) if query.live => self.view_cadence,
            ResourceKey::Commissions => self.view_cadence,
            ResourceKey::Journal(_)
            | ResourceKey::Artifact(_)
            | ResourceKey::Transcript(_)
            | ResourceKey::MetricsTimeline(_)
            | ResourceKey::MetricsSeats
            | ResourceKey::MetricsDispatches
            | ResourceKey::Commission(_) => Duration::ZERO,
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
            ResourceKey::MetricsSummary => self.polled_due(&self.summary),
            ResourceKey::MetricsDays => self.polled_due(&self.days),
            ResourceKey::Spend => self.polled_due(&self.spend),
            ResourceKey::MetricsSeats => self.seats.on_demand_due(),
            ResourceKey::MetricsDispatches => self.dispatches.on_demand_due(),
            ResourceKey::MetricsTimeline(bloom) => self.timelines.get(bloom).is_none_or(Cell::on_demand_due),
            ResourceKey::Commissions => {
                if self.commission_capability == Some(CommissionCapability::Absent) {
                    return false;
                }
                self.polled_due(&self.commissions)
            }
            ResourceKey::Commission(id) => {
                if self.commission_capability == Some(CommissionCapability::Absent) {
                    return false;
                }
                self.commission_shows.get(id).is_none_or(Cell::on_demand_due)
            }
        }
    }

    fn polled_due<T>(&self, cell: &Cell<T>) -> bool {
        !cell.inflight && cell.completed_at.is_none_or(|at| at.elapsed() >= self.view_cadence)
    }

    #[must_use]
    pub fn is_inflight(&self, key: &ResourceKey) -> bool {
        match key {
            ResourceKey::View => self.view.inflight,
            ResourceKey::Journal(query) => self.journals.get(query).is_some_and(|cell| cell.inflight),
            ResourceKey::Artifact(digest) => self.artifacts.get(digest).is_some_and(|cell| cell.inflight),
            ResourceKey::Transcript(query) => self.transcripts.get(query).is_some_and(|cell| cell.inflight),
            ResourceKey::MetricsSummary => self.summary.inflight,
            ResourceKey::MetricsDays => self.days.inflight,
            ResourceKey::MetricsTimeline(bloom) => self.timelines.get(bloom).is_some_and(|cell| cell.inflight),
            ResourceKey::MetricsSeats => self.seats.inflight,
            ResourceKey::MetricsDispatches => self.dispatches.inflight,
            ResourceKey::Spend => self.spend.inflight,
            ResourceKey::Commissions => self.commissions.inflight,
            ResourceKey::Commission(id) => self.commission_shows.get(id).is_some_and(|cell| cell.inflight),
        }
    }

    pub fn mark_inflight(&mut self, key: &ResourceKey) {
        match key {
            ResourceKey::View => self.view.inflight = true,
            ResourceKey::Journal(query) => self.journals.entry(*query).or_default().inflight = true,
            ResourceKey::Artifact(digest) => self.artifacts.entry(*digest).or_default().inflight = true,
            ResourceKey::Transcript(query) => self.transcripts.entry(query.clone()).or_default().inflight = true,
            ResourceKey::MetricsSummary => self.summary.inflight = true,
            ResourceKey::MetricsDays => self.days.inflight = true,
            ResourceKey::MetricsTimeline(bloom) => self.timelines.entry(*bloom).or_default().inflight = true,
            ResourceKey::MetricsSeats => self.seats.inflight = true,
            ResourceKey::MetricsDispatches => self.dispatches.inflight = true,
            ResourceKey::Spend => self.spend.inflight = true,
            ResourceKey::Commissions => self.commissions.inflight = true,
            ResourceKey::Commission(id) => self.commission_shows.entry(id.clone()).or_default().inflight = true,
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

    pub fn apply_summary(&mut self, result: Result<MetricsSummary, String>) {
        apply(&mut self.summary, result);
    }

    pub fn apply_days(&mut self, result: Result<Vec<MetricDay>, String>) {
        apply(&mut self.days, result);
    }

    pub fn apply_timeline(&mut self, bloom: DigestHex, result: Result<MetricsTimeline, String>) {
        apply(self.timelines.entry(bloom).or_default(), result);
    }

    pub fn apply_seats(&mut self, result: Result<Vec<MetricsSeat>, String>) {
        apply(&mut self.seats, result);
    }

    pub fn apply_dispatches(&mut self, result: Result<Vec<MetricDispatch>, String>) {
        apply(&mut self.dispatches, result);
    }

    pub fn apply_spend(&mut self, result: Result<SpendWindowView, String>) {
        apply(&mut self.spend, result);
    }

    pub fn apply_commissions(&mut self, result: Result<CommissionsView, String>) {
        match result {
            Ok(value) => {
                self.commission_capability = Some(CommissionCapability::Present);
                self.commissions.apply_ok(value);
            }
            Err(error) => self.commissions.apply_err(error),
        }
    }

    pub fn apply_commissions_missing(&mut self) {
        self.commission_capability = Some(CommissionCapability::Absent);
        self.commissions.settle();
    }

    pub fn apply_commission(&mut self, id: String, result: Result<CommissionShowView, String>) {
        apply(self.commission_shows.entry(id).or_default(), result);
    }

    pub fn apply_err(&mut self, key: &ResourceKey, error: impl Display) {
        match key {
            ResourceKey::View => self.view.apply_err(error),
            ResourceKey::Journal(query) => self.journals.entry(*query).or_default().apply_err(error),
            ResourceKey::Artifact(digest) => self.artifacts.entry(*digest).or_default().apply_err(error),
            ResourceKey::Transcript(query) => self.transcripts.entry(query.clone()).or_default().apply_err(error),
            ResourceKey::MetricsSummary => self.summary.apply_err(error),
            ResourceKey::MetricsDays => self.days.apply_err(error),
            ResourceKey::MetricsTimeline(bloom) => self.timelines.entry(*bloom).or_default().apply_err(error),
            ResourceKey::MetricsSeats => self.seats.apply_err(error),
            ResourceKey::MetricsDispatches => self.dispatches.apply_err(error),
            ResourceKey::Spend => self.spend.apply_err(error),
            ResourceKey::Commissions => self.commissions.apply_err(error),
            ResourceKey::Commission(id) => self.commission_shows.entry(id.clone()).or_default().apply_err(error),
        }
    }
}

fn apply<T>(cell: &mut Cell<T>, result: Result<T, String>) {
    match result {
        Ok(value) => cell.apply_ok(value),
        Err(error) => cell.apply_err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{CommissionCapability, ResourceKey, Store};
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

    #[test]
    fn a_missing_commission_route_is_cached_and_does_not_clear_the_board() {
        // The plausible bug: a 404 on /commissions is retried every cadence
        // (or treated as a store error that dims /view), so a predating
        // coordinator keeps failing and the board goes stale with it.
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
        assert!(store.due(&ResourceKey::Commissions));
        store.apply_commissions_missing();
        assert_eq!(store.commission_capability(), Some(CommissionCapability::Absent));
        assert!(!store.due(&ResourceKey::Commissions));
        assert!(!store.due(&ResourceKey::Commission("wp-a".to_owned())));
        assert!(!store.view().is_stale());
        assert_eq!(store.view().value.as_ref().map(|view| view.blooms.len()), Some(1));
    }
}
