//! Read-only boot-path sweep for `--check-store` (issue #6024).
//!
//! Decoding every journal row proves the rows boot replays, but boot reads
//! far more than the journal: commission heads and the scope-run ledger, the
//! ADR tables, outstanding orders, construct sessions, the shared-run
//! projection, proof facts, metrics, and the outbox. A column those readers
//! name but the store lacks — `scope_runs.model_override` on 2026-09-15 —
//! sailed through the decode proof and failed the boot.
//!
//! [`SqliteStore::read_everything`] closes that gap: one pass over each
//! backend's list/lookup path, read-only, tallying rows per table and
//! collecting every refusal instead of stopping at the first. Each probe
//! calls the real reader, so its `SELECT` names the real columns — a
//! `SELECT *` would not fail on a dropped column — while the trailing count
//! loop tallies rows the keyed probes cannot. A table may therefore carry
//! both a tally and a refusal: the count proves the rows are there, the
//! refusal that the reader cannot parse them.
//!
//! Keyed tables whose only reader needs a live key (`commission_statements`,
//! `scope_verify_reports`, `adr_transitions`, `outbox_results`) are counted
//! always and probed whenever a live key exists — the per-commission,
//! per-run, and first-outbox-row iteration below — so a populated store
//! exercises their decode paths too. An empty store leaves them to the
//! count loop; the coverage test's allowlist names only the `SQLite`
//! internals no backend read can name.

use std::collections::BTreeMap;
use std::fmt;

use aether_bloomery::{Digest, WorkpieceId};

use super::{AdrBackend, CommissionBackend, SqliteStore, StoreBackend, now_unix_millis};

/// Key the point-lookup probes read under. Absent by construction, so each
/// one misses after its `SELECT` has named the columns — preparing the
/// statement is the proof.
const PROBE_NONCE: &str = "__read_everything_probe__";
const PROBE_WORKPIECE: &str = "__read_everything_probe__";
const PROBE_TOPIC: &str = "__read_everything_probe__";
const PROBE_SLUG: &str = "__read_everything_probe__";
/// Opaque bytes for every `&[u8]`-keyed probe (blooms, runs, plans).
const PROBE_BYTES: &[u8] = b"__read_everything_probe__";
/// A digest no writer ever stored, for the digest-keyed commission and ADR probes.
const PROBE_DIGEST: Digest = Digest::from_bytes([0xA5; 32]);

/// What the sweep found: rows per table and every read refusal.
///
/// A refusal names its table (`scope_runs: no such column:
/// model_override`), the way decode refusals name their row.
#[derive(Debug, Default)]
pub struct ReadTally {
    /// Rows per table, in [`READ_TABLES`] order.
    pub rows: BTreeMap<String, usize>,
    /// One line per backend read that refused, in sweep order.
    pub refusals: Vec<String>,
}

impl ReadTally {
    /// No backend read refused.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.refusals.is_empty()
    }

    /// Record one backend read: a refusal becomes a named line, a success
    /// needs nothing further (the count loop tallies every table).
    fn probe<T, E: fmt::Display>(&mut self, table: &'static str, result: Result<T, E>) {
        if let Err(error) = result {
            self.refusals.push(format!("{table}: {error}"));
        }
    }

    /// Record one backend read and hand its value back for live-key
    /// iteration; a refusal becomes a named line and reads as no keys.
    fn checked<T, E: fmt::Display>(&mut self, table: &'static str, result: Result<T, E>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.refusals.push(format!("{table}: {error}"));
                None
            }
        }
    }

    /// Tally one table's rows. Runs after the probes, so a table the
    /// readers cannot parse still reports how many rows it holds.
    fn count(&mut self, store: &SqliteStore, table: &'static str) {
        match store.conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_, i64>(0)) {
            Ok(count) => {
                self.rows.insert(table.to_owned(), usize::try_from(count).unwrap_or_default());
            }
            Err(error) => self.refusals.push(format!("{table}: {error}")),
        }
    }
}

/// Every data table the sweep names. The coverage test fails when
/// `sqlite_master` holds a table missing here, so a new table cannot land
/// without joining the sweep.
const READ_TABLES: &[&str] = &[
    "journal",
    "active_membership",
    "config",
    "outbox",
    "outbox_results",
    "outstanding_orders",
    "parked_question",
    "dispatch_owners",
    "capture_diff",
    "fold_conflict",
    "study_index",
    "dispatch_description",
    "authorized_instructions",
    "review_findings",
    "candidate_commit_message",
    "candidate_hash",
    "suppression_request",
    "proof_facts",
    "flake_registry",
    "metric_dispatch",
    "metric_bloom",
    "metric_day",
    "metric_cursor",
    "construct_session",
    "member_dependency",
    "notification_sent",
    "shared_runs",
    "shared_run_members",
    "shared_run_steps",
    "contextual_dispatches",
    "shared_member_verification_queue",
    "shared_run_cancellations",
    "partial_head_repairs",
    "construction_admissions",
    "shared_run_proof_reuse",
    "commissions",
    "commission_statements",
    "scope_revisions",
    "commission_approvals",
    "commission_projections",
    "scope_verify_reports",
    "scope_runs",
    "adrs",
    "adr_transitions",
];

impl SqliteStore {
    /// Call each backend's list/lookup path once, read-only, tallying rows
    /// per table and collecting every refusal.
    ///
    /// Runs after the migration opening the store performs, the way boot
    /// would — so a column the readers name but the store lacks fails here,
    /// not at restart. Never writes: the metrics fold and every
    /// record/mark/consume path are deliberately absent. Always returns
    /// `Ok`; per-table failures collect into the tally, and the `Result`
    /// keeps a future fatal open error from reshaping the proof.
    pub fn read_everything(&mut self) -> rusqlite::Result<ReadTally> {
        let mut tally = ReadTally::default();
        self.read_journal_and_config(&mut tally);
        self.read_outbox(&mut tally);
        self.read_orders(&mut tally);
        self.read_metrics_and_notify(&mut tally);
        self.read_sessions(&mut tally);
        self.read_shared_runs(&mut tally);
        self.read_commissions(&mut tally);
        self.read_adrs(&mut tally);
        for &table in READ_TABLES {
            tally.count(self, table);
        }
        Ok(tally)
    }

    /// The recovery replay source, its narrow readers, the config set, and
    /// the membership registry.
    fn read_journal_and_config(&mut self, tally: &mut ReadTally) {
        tally.probe("journal", self.replay_journal());
        tally.probe("journal", self.list_events());
        tally.probe("journal", self.journal_recorded_unix_millis());
        tally.probe("journal", self.journal_holds_any(&[PROBE_NONCE.to_owned()]));
        tally.probe("config", self.load_configs());
        tally.probe("active_membership", self.holds_member_membership(PROBE_BYTES, PROBE_WORKPIECE));
    }

    /// The outbox drain pair plus the sidecar read for the first undelivered
    /// row, when one exists — the sidecar refuses unknown keys by name, so
    /// an empty outbox offers no probe key and leaves it to the count loop.
    fn read_outbox(&mut self, tally: &mut ReadTally) {
        if let Some(drained) = tally.checked("outbox", self.drain_outbox(None))
            && let Some(first) = drained.into_iter().next()
        {
            tally.probe("outbox_results", self.outbox_results(&first.topic, first.sequence));
        }
        tally.probe("outbox", self.delivered_outbox(PROBE_TOPIC));
        tally.probe("outbox", self.has_outbox_topic(PROBE_TOPIC));
    }

    /// Outstanding orders and every small registry keyed like one.
    fn read_orders(&mut self, tally: &mut ReadTally) {
        tally.probe("outstanding_orders", self.list_expired_orders(now_unix_millis()));
        tally.probe("outstanding_orders", self.list_outstanding_nonces());
        tally.probe("outstanding_orders", self.list_order_nonces());
        tally.probe("outstanding_orders", self.list_live_orders());
        tally.probe("outstanding_orders", self.list_bloom_dispatch_live(PROBE_BYTES));
        tally.probe("outstanding_orders", self.lookup_order(PROBE_NONCE));
        tally.probe("outstanding_orders", self.lookup_named_dispatch(PROBE_NONCE));
        tally.probe("dispatch_owners", self.lookup_dispatch_owner(PROBE_NONCE));
        tally.probe("parked_question", self.lookup_parked_question(PROBE_BYTES, PROBE_BYTES));
        tally.probe("study_index", self.study_rows());
        tally.probe("study_index", self.lookup_study(PROBE_BYTES, PROBE_BYTES));
        tally.probe("dispatch_description", self.list_dispatch_descriptions(PROBE_BYTES));
        tally.probe("authorized_instructions", self.list_authorized_instructions());
        tally.probe("authorized_instructions", self.instructions_authorized(PROBE_BYTES));
        tally.probe("review_findings", self.lookup_review_findings(PROBE_BYTES, PROBE_WORKPIECE));
        tally.probe("candidate_commit_message", self.lookup_candidate_commit_message(PROBE_BYTES, PROBE_WORKPIECE));
        tally.probe("candidate_hash", self.latest_candidate_hash(PROBE_WORKPIECE));
        tally.probe("suppression_request", self.list_suppression_requests(PROBE_BYTES));
        tally.probe("capture_diff", self.lookup_capture_diff(PROBE_NONCE));
        tally.probe("fold_conflict", self.lookup_fold_conflict(PROBE_BYTES, PROBE_WORKPIECE));
        tally.probe("proof_facts", self.list_proof_facts());
        tally.probe("flake_registry", self.known_flakes(2));
    }

    /// The metrics cache slot, the determinism oracle, and operator notifications.
    fn read_metrics_and_notify(&mut self, tally: &mut ReadTally) {
        tally.probe("metric_dispatch", self.metric_dispatch_payloads());
        tally.probe("metric_dispatch", self.list_bloom_dispatch_rollup(PROBE_BYTES));
        tally.probe("metric_cursor", self.metrics_cursor());
        tally.probe("notification_sent", self.list_notifications());
    }

    /// The construct-session registry and the sealed member graph.
    fn read_sessions(&mut self, tally: &mut ReadTally) {
        tally.probe("construct_session", self.lookup_construct_session_meta(PROBE_BYTES, PROBE_WORKPIECE));
        tally.probe("construct_session", self.lookup_session_owner(PROBE_SLUG));
        tally.probe("construct_session", self.lookup_session_slug(PROBE_BYTES, PROBE_WORKPIECE));
        tally.probe("construct_session", self.construct_session_holder(PROBE_BYTES, PROBE_NONCE));
        tally.probe("construct_session", self.session_slug_holder(PROBE_BYTES, PROBE_SLUG, PROBE_WORKPIECE));
        tally.probe("member_dependency", self.lookup_predecessors(PROBE_BYTES, PROBE_WORKPIECE));
    }

    /// The shared-run projection: whole-table lists, one keyed probe per
    /// table, and the per-run member/step reads restart recovery performs.
    fn read_shared_runs(&mut self, tally: &mut ReadTally) {
        if let Some(runs) = tally.checked("shared_runs", self.list_shared_runs()) {
            for run in &runs {
                tally.probe("shared_run_members", self.shared_run_members(&run.run));
                tally.probe("shared_run_steps", self.shared_run_steps(&run.run));
            }
        }
        tally.probe("shared_runs", self.list_open_shared_runs());
        tally.probe("shared_runs", self.lookup_shared_run(PROBE_BYTES));
        tally.probe("shared_run_members", self.shared_run_members(PROBE_BYTES));
        tally.probe("shared_run_steps", self.shared_run_steps(PROBE_BYTES));
        tally.probe("shared_run_steps", self.list_unprepared_shared_run_steps(1));
        tally.probe("shared_run_steps", self.shared_step_physical_run(PROBE_NONCE));
        tally.probe("contextual_dispatches", self.lookup_contextual_dispatch(PROBE_NONCE));
        tally.probe("shared_member_verification_queue", self.queued_member_verifications());
        tally.probe("shared_member_verification_queue", self.queued_member_verification(PROBE_BYTES));
        tally.probe("shared_run_cancellations", self.shared_run_cancelled(PROBE_BYTES));
        tally.probe("partial_head_repairs", self.list_partial_head_repairs());
        tally.probe("construction_admissions", self.construction_admission(PROBE_BYTES));
        tally.probe("construction_admissions", self.has_pending_construction_admission());
        tally.probe("shared_run_proof_reuse", self.shared_run_proof_reuse(PROBE_BYTES));
    }

    /// Commissions with their scope runs, revisions, and approvals: dummy
    /// probes for the column lists, then the per-commission reads boot walks.
    fn read_commissions(&mut self, tally: &mut ReadTally) {
        tally.probe("scope_runs", self.list_scope_runs(PROBE_WORKPIECE));
        tally.probe("scope_runs", self.list_unfrozen_scope_verdicts());
        tally.probe("scope_runs", self.next_scope_run_ordinal(PROBE_WORKPIECE));
        tally.probe("scope_runs", self.lookup_scope_run(PROBE_NONCE));
        tally.probe("scope_revisions", CommissionBackend::load_revision(self, PROBE_DIGEST));
        tally.probe("commission_approvals", CommissionBackend::load_approvals(self, PROBE_DIGEST));
        tally.probe(
            "commission_projections",
            CommissionBackend::load_projection(self, &WorkpieceId(PROBE_WORKPIECE.to_owned())),
        );
        tally.probe("commissions", self.commission_is_resolved(PROBE_WORKPIECE));
        if let Some(heads) = tally.checked("commissions", CommissionBackend::list(self, None)) {
            for head in &heads {
                tally.probe("scope_runs", self.list_scope_runs(&head.id.0));
                tally.probe("commissions", CommissionBackend::load(self, &head.id));
                tally.probe("commission_projections", CommissionBackend::load_projection(self, &head.id));
                if let Some(tip) = head.current_revision {
                    tally.probe("scope_revisions", CommissionBackend::load_revision(self, tip));
                    tally.probe("commission_approvals", CommissionBackend::load_approvals(self, tip));
                }
            }
        }
    }

    /// Every stored ADR with its status view, plus the digest and number
    /// lookups on a digest that is absent.
    fn read_adrs(&mut self, tally: &mut ReadTally) {
        tally.probe("adrs", AdrBackend::list(self));
        tally.probe("adrs", AdrBackend::load(self, PROBE_DIGEST));
        tally.probe("adrs", AdrBackend::load_by_number(self, 0));
    }
}
