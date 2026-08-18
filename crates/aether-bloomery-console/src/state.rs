//! Snapshot → rows, alert extraction, selection identity, staleness clock.

use std::fmt::Display;
use std::time::{Duration, Instant};

use crate::dto::{BloomStatus, BloomView, DigestHex, MemberView, ViewDocument};

/// Stable identity of one selectable row. Refreshes look this up so the
/// cursor does not walk out from under the operator when `/view` reorders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowId {
    Bloom { id: DigestHex },
    Member { bloom: DigestHex, workpiece: String },
}

/// One rendered row on the board.
#[derive(Clone, Debug)]
pub enum BoardRow {
    Bloom(BloomRow),
    Member(MemberRow),
}

/// A bloom header row.
#[derive(Clone, Debug)]
pub struct BloomRow {
    pub id: DigestHex,
    pub id_prefix: String,
    pub status: String,
    pub member_count: usize,
}

/// A member row under its bloom.
#[derive(Clone, Debug)]
pub struct MemberRow {
    pub bloom: DigestHex,
    pub workpiece: String,
    pub state: String,
    pub machinery: String,
    pub blocked_by: String,
    pub wedge_cause: String,
}

impl BoardRow {
    #[must_use]
    pub fn id(&self) -> RowId {
        match self {
            Self::Bloom(row) => RowId::Bloom { id: row.id },
            Self::Member(row) => RowId::Member { bloom: row.bloom, workpiece: row.workpiece.clone() },
        }
    }
}

/// One loud token in the alert band.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub token: String,
    pub detail: String,
}

/// The board the UI paints each frame.
#[derive(Clone, Debug)]
pub struct BoardState {
    pub rows: Vec<BoardRow>,
    pub alerts: Vec<Alert>,
    pub selected: Option<RowId>,
    pub last_ok: Option<Instant>,
    pub last_error: Option<String>,
    pub endpoint_label: String,
}

impl BoardState {
    #[must_use]
    pub fn new(endpoint_label: String) -> Self {
        Self { rows: Vec::new(), alerts: Vec::new(), selected: None, last_ok: None, last_error: None, endpoint_label }
    }

    pub fn apply_view(&mut self, view: &ViewDocument) {
        self.rows = rows_of(view);
        self.alerts = alerts_of(view);
        self.last_ok = Some(Instant::now());
        self.last_error = None;
        self.reseat_selection();
    }

    pub fn apply_error(&mut self, error: impl Display) {
        self.last_error = Some(error.to_string());
    }

    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.last_error.is_some()
    }

    #[must_use]
    pub fn sample_age(&self) -> Option<Duration> {
        self.last_ok.map(|when| when.elapsed())
    }

    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.rows.iter().position(|row| row.id() == *id)
    }

    pub fn select_next(&mut self) {
        match self.selected_index() {
            Some(index) => {
                if let Some(row) = self.rows.get(index + 1) {
                    self.selected = Some(row.id());
                }
            }
            None => self.selected = self.rows.first().map(BoardRow::id),
        }
    }

    pub fn select_prev(&mut self) {
        match self.selected_index() {
            Some(0) | None => self.selected = self.rows.first().map(BoardRow::id),
            Some(index) => self.selected = self.rows.get(index - 1).map(BoardRow::id),
        }
    }

    fn reseat_selection(&mut self) {
        if let Some(id) = &self.selected
            && self.rows.iter().any(|row| row.id() == *id)
        {
            return;
        }
        if let Some(RowId::Member { bloom, .. }) = &self.selected {
            let bloom = *bloom;
            if self.rows.iter().any(|row| matches!(row, BoardRow::Bloom(row) if row.id == bloom)) {
                self.selected = Some(RowId::Bloom { id: bloom });
                return;
            }
        }
        self.selected = self.rows.first().map(BoardRow::id);
    }
}

/// The one-word state `scripts/bloomery-operator.py`'s `member_status_state`
/// prints. `has_order` is the outstanding-order bit that script reads from
/// the journal; the console is `/view`-only, so the live board always
/// passes `false`.
#[must_use]
pub fn member_status_state(member: &MemberView, has_order: bool) -> &'static str {
    if member.wedge.is_some() {
        return "WEDGED";
    }
    if member.pending_decision.is_some() {
        return "held";
    }
    if member.resolution.is_some() {
        return "integrated";
    }
    if has_order {
        return "running";
    }
    if member.blocked_by.as_deref().is_some_and(|name| !name.is_empty()) {
        return "blocked";
    }
    "idle"
}

fn rows_of(view: &ViewDocument) -> Vec<BoardRow> {
    let mut rows = Vec::new();
    for bloom in &view.blooms {
        rows.push(BoardRow::Bloom(BloomRow {
            id: bloom.id,
            id_prefix: bloom.id.prefix(),
            status: bloom_status_label(bloom.status),
            member_count: bloom.members.len(),
        }));
        for member in &bloom.members {
            rows.push(BoardRow::Member(MemberRow {
                bloom: bloom.id,
                workpiece: member.workpiece.clone(),
                state: member_status_state(member, false).to_owned(),
                machinery: format!("{}/{}", member.machinery_rolls, member.machinery_budget),
                blocked_by: member.blocked_by.clone().filter(|name| !name.is_empty()).unwrap_or_default(),
                wedge_cause: member.wedge_cause.map_or_else(String::new, |cause| cause.to_string()),
            }));
        }
    }
    rows
}

fn alerts_of(view: &ViewDocument) -> Vec<Alert> {
    let mut alerts = Vec::new();
    for bloom in &view.blooms {
        let prefix = bloom.id.prefix();
        if bloom.review_park.is_some() {
            alerts.push(Alert { token: "PARK".to_owned(), detail: prefix.clone() });
        }
        if let Some(block) = &bloom.landing_blocked {
            alerts.push(Alert {
                token: format!("land: blocked {}/{}", block.rolls, block.budget),
                detail: prefix.clone(),
            });
        }
        if let Some(fault) = &bloom.executor_fault {
            alerts
                .push(Alert { token: executor_fault_token(fault.rolls, fault.budget, fault.terminal), detail: prefix });
        }
        for member in &bloom.members {
            push_member_alerts(&mut alerts, bloom, member);
        }
    }
    alerts
}

fn push_member_alerts(alerts: &mut Vec<Alert>, bloom: &BloomView, member: &MemberView) {
    let detail = if member.workpiece.is_empty() {
        bloom.id.prefix()
    } else {
        member.workpiece.clone()
    };
    if member.wedge.is_some() {
        alerts.push(Alert { token: "WEDGED".to_owned(), detail: detail.clone() });
    }
    if member.host_fault.is_some() {
        alerts.push(Alert { token: "hostfault".to_owned(), detail });
    }
}

fn executor_fault_token(rolls: u32, budget: u32, terminal: bool) -> String {
    let mut token = format!("FAULT {rolls}/{budget}");
    if terminal {
        token.push_str(" TERMINAL");
    }
    token
}

fn bloom_status_label(status: Option<BloomStatus>) -> String {
    status.map_or_else(|| "?".to_owned(), |status| status.to_string())
}

/// Age of the last successful sample, for the header.
#[must_use]
pub fn format_age(age: Option<Duration>) -> String {
    let Some(age) = age else {
        return "never".to_owned();
    };
    let secs = age.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    format!("{}h", mins / 60)
}

#[cfg(test)]
mod tests {
    use super::{BoardRow, BoardState, RowId, alerts_of, format_age, member_status_state, rows_of};
    use crate::dto::{
        BloomStatus, BloomView, DigestHex, ExecutorFaultView, LandingBlock, MemberView, PendingDecisionView, Present,
        ReviewParkView, ViewDocument, WedgeCause,
    };
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn member(workpiece: &str) -> MemberView {
        MemberView { workpiece: workpiece.to_owned(), ..MemberView::default() }
    }

    #[test]
    fn member_status_state_matches_the_operator_script() {
        // The plausible bug: a dependent carrying blocked_by paints as idle
        // (the mysterious idleness the readiness scheduler exists to name),
        // or a wedge loses to blocked_by / resolution.
        assert_eq!(
            member_status_state(&MemberView { blocked_by: Some("wp-a".to_owned()), ..member("wp") }, false),
            "blocked"
        );
        assert_eq!(member_status_state(&member("wp"), true), "running");
        assert_eq!(
            member_status_state(
                &MemberView { resolution: Some(Present {}), blocked_by: Some("wp-a".to_owned()), ..member("wp") },
                false,
            ),
            "integrated"
        );
        assert_eq!(
            member_status_state(
                &MemberView { wedge: Some(Present {}), blocked_by: Some("wp-a".to_owned()), ..member("wp") },
                false,
            ),
            "WEDGED"
        );
        assert_eq!(
            member_status_state(
                &MemberView { pending_decision: Some(PendingDecisionView::default()), ..member("wp") },
                false,
            ),
            "held"
        );
        assert_eq!(member_status_state(&MemberView { blocked_by: Some(String::new()), ..member("wp") }, false), "idle");
        assert_eq!(member_status_state(&member("wp"), false), "idle");
    }

    #[test]
    fn alerts_name_every_loud_state() {
        // The plausible bug: the band only looks at bloom-level fields, so a
        // wedged or host-faulted member stays quiet; or TERMINAL is dropped.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(0xab),
                review_park: Some(ReviewParkView::default()),
                landing_blocked: Some(LandingBlock { rolls: 2, budget: 3 }),
                executor_fault: Some(ExecutorFaultView { rolls: 3, budget: 3, terminal: true }),
                members: vec![
                    MemberView { wedge: Some(Present {}), wedge_cause: Some(WedgeCause::Work), ..member("issue-1") },
                    MemberView { host_fault: Some(Present {}), ..member("issue-2") },
                ],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let tokens: Vec<_> = alerts_of(&view).into_iter().map(|alert| alert.token).collect();
        assert_eq!(tokens, ["PARK", "land: blocked 2/3", "FAULT 3/3 TERMINAL", "WEDGED", "hostfault"]);
    }

    #[test]
    fn selection_stays_on_the_workpiece_across_a_reorder() {
        // The plausible bug: j/k is stored as a row index, so a refresh that
        // inserts a bloom above the cursor silently moves the highlight.
        let first = ViewDocument {
            blooms: vec![
                BloomView { id: digest(1), members: vec![member("wp-a"), member("wp-b")], ..BloomView::default() },
                BloomView { id: digest(2), members: vec![member("wp-c")], ..BloomView::default() },
            ],
            ..ViewDocument::default()
        };
        let mut state = BoardState::new("127.0.0.1:8910".to_owned());
        state.apply_view(&first);
        state.selected = Some(RowId::Member { bloom: digest(1), workpiece: "wp-b".to_owned() });
        assert_eq!(state.selected_index(), Some(2));

        let reordered = ViewDocument {
            blooms: vec![
                BloomView { id: digest(2), members: vec![member("wp-c")], ..BloomView::default() },
                BloomView { id: digest(1), members: vec![member("wp-b"), member("wp-a")], ..BloomView::default() },
            ],
            ..ViewDocument::default()
        };
        state.apply_view(&reordered);
        assert_eq!(state.selected, Some(RowId::Member { bloom: digest(1), workpiece: "wp-b".to_owned() }));
        assert_eq!(state.selected_index(), Some(3));
    }

    #[test]
    fn a_failed_poll_keeps_the_last_board() {
        // The plausible bug: a connection-refused mid-run clears the rows,
        // so a coordinator restart blanks the board instead of dimming it.
        let view = ViewDocument {
            blooms: vec![BloomView { id: digest(1), members: vec![member("wp-a")], ..BloomView::default() }],
            ..ViewDocument::default()
        };
        let mut state = BoardState::new("127.0.0.1:8910".to_owned());
        state.apply_view(&view);
        assert_eq!(state.rows.len(), 2);
        assert!(!state.is_stale());
        state.apply_error("connection refused");
        assert!(state.is_stale());
        assert_eq!(state.rows.len(), 2);
        assert_eq!(state.last_error.as_deref(), Some("connection refused"));
        assert!(state.last_ok.is_some());
    }

    #[test]
    fn rows_carry_machinery_blocker_and_wedge_cause() {
        // The plausible bug: the table prints bloom status and workpiece
        // only, so a machinery wedge and its blocked_by ancestor never reach
        // the operator.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                status: Some(BloomStatus::Sealed),
                members: vec![MemberView {
                    machinery_rolls: 2,
                    machinery_budget: 3,
                    blocked_by: Some("wp-a".to_owned()),
                    wedge: Some(Present {}),
                    wedge_cause: Some(WedgeCause::Machinery),
                    ..member("wp-b")
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let rows = rows_of(&view);
        assert_eq!(rows.len(), 2);
        let BoardRow::Member(member) = &rows[1] else {
            panic!("second row is the member");
        };
        assert_eq!(member.state, "WEDGED");
        assert_eq!(member.machinery, "2/3");
        assert_eq!(member.blocked_by, "wp-a");
        assert_eq!(member.wedge_cause, "Machinery");
    }

    #[test]
    fn format_age_names_never_before_the_first_sample() {
        assert_eq!(format_age(None), "never");
        assert_eq!(format_age(Some(Duration::from_secs(0))), "0s");
        assert_eq!(format_age(Some(Duration::from_secs(59))), "59s");
        assert_eq!(format_age(Some(Duration::from_mins(1))), "1m");
        assert_eq!(format_age(Some(Duration::from_hours(1))), "1h");
    }
}
