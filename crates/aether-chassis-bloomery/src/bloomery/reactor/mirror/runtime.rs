//! The runtime for the mirror outbox reactor capability (ADR-0149
//! migration step 1, third slice — issue #3499).
//!
//! `ProjectionShell` and `SourceShell` wrap the GitHub backends behind an
//! `Arc<dyn …>` boundary, but until this slice nothing in the running process
//! ever called them. This capability mounts both shells as reactors of the
//! store's transactional outbox: a poll-driven drain routes each undelivered
//! entry through the right shell (view documents reconcile the outward mirror,
//! landing receipts project outward), and the delivered prefix is acked **only
//! after the GitHub write succeeds**. Drain is non-destructive and ack is
//! monotonic within a topic, so a crash between project and ack simply
//! re-delivers on the next boot — the boot-time republish is this capability's
//! first drain pass, and the mirror's idempotent reconcile absorbs the
//! re-delivery (at-least-once with idempotent reconcile).
//!
//! That idempotency is find-then-create against a remote, which converges only
//! when the writes are **serial**: two workers racing over one entry both read
//! "absent" and both create. So a drain cycle runs alone — a tick that arrives
//! while drains are still owed a reply, or while a projection is still out on
//! the network, is skipped rather than re-driving the same undelivered entry.
//! The poll interval means "at most one cycle in flight", not "start a cycle".
//!
//! Ownership (ADR-0149 §Outbox consumption): this capability is the **sole
//! reactor of the projection topics** `view_document` and `landing_receipt` —
//! it alone drains and acks them, scoped by topic so a future executor-dispatch
//! reactor (#3505) coexists without racing on the shared `delivered` flag. The
//! producer (#3497) only enqueues; it never acks.
//!
//! Config-gated: when the mirror is unconfigured (empty token / owner / repo)
//! the capability mounts disabled — no shells, no timer, no drain — so an
//! un-tokened dev boot neither errors nor spins a stuck loop; the outbox simply
//! accumulates and republishes once configured. The source shell is mounted for
//! recovery-drill completeness but dormant: step 1 produces no source-driven
//! topics (`cas_land_enabled` off, no execution, no landing), so only the two
//! projection topics are drained.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aether_actor::{MailSender, runtime};
use aether_bloomery::{CommissionProjection, ProjectedReceipt, Topic, ViewDocument};
use aether_bloomery_github::ReplicaError;
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::MirrorReactorCapability;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::{MirrorReactorSetup, ProjectionShell, SourceReplicaShell, SourceShell};
use crate::store::{
    AckOutbox, AckOutboxResult, DrainOutbox, DrainOutboxResult, OutboxEntry, RecordCommissionProjection,
    RecordCommissionProjectionResult, StoreCapability,
};

/// The self-addressed wake the poll timer fires each interval; its handler
/// drains the store outbox. Zero-field — the timer carries only the schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.mirror.drain_tick")]
pub struct DrainTick {}

/// Runtime state for [`MirrorReactorCapability`]. The shells are `Some` only when
/// the mirror is configured; a disabled reactor holds neither and spawns no
/// timer. `mailer` + `self_mailbox` are the loopback wake handle `wire` uses to
/// fire the immediate boot-republish tick.
pub struct MirrorReactorState {
    projection: Option<ProjectionShell>,
    // Mounted for recovery-drill completeness; dormant until the step-2
    // executor bridge produces source-driven topics.
    _source: Option<SourceShell>,
    replica: Option<SourceReplicaShell>,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
    // Drain replies still owed by the drains the current cycle sent.
    awaited_drains: usize,
    // Projection workers still out. Shared with the detached workers, which
    // release their slot on completion.
    outstanding: Arc<AtomicUsize>,
}

/// A detached worker's claim on the drain cycle, released on drop so a worker
/// that panics mid-projection still frees the cycle rather than wedging the
/// reactor at "permanently busy".
struct WorkerSlot(Arc<AtomicUsize>);

impl WorkerSlot {
    fn claim(outstanding: &Arc<AtomicUsize>) -> Self {
        outstanding.fetch_add(1, Ordering::Release);
        Self(Arc::clone(outstanding))
    }
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl MirrorReactorState {
    /// Build state over explicit shells — the seam the handler / integration
    /// tests drive with fake-GitHub-backed shells, bypassing `connect` (which
    /// needs a real network). Spawns no timer; tests drive the drain by feeding
    /// a [`DrainTick`] or a [`DrainOutboxResult`] into dispatch directly.
    #[must_use]
    pub fn with_shells(
        projection: Option<ProjectionShell>,
        source: Option<SourceShell>,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    ) -> Self {
        Self::with_replica(projection, source, None, mailer, self_mailbox)
    }

    /// Same as [`with_shells`](Self::with_shells) plus an optional source-replica
    /// backend.
    #[must_use]
    pub fn with_replica(
        projection: Option<ProjectionShell>,
        source: Option<SourceShell>,
        replica: Option<SourceReplicaShell>,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    ) -> Self {
        Self {
            projection,
            _source: source,
            replica,
            mailer,
            self_mailbox,
            _timer: None,
            awaited_drains: 0,
            outstanding: Arc::default(),
        }
    }

    /// Whether a drain cycle is still running: drains whose replies have not
    /// arrived, or projections still out on the network.
    fn cycle_in_flight(&self) -> bool {
        self.awaited_drains > 0 || self.outstanding.load(Ordering::Acquire) > 0
    }
}

/// Decode `entry`'s payload for its topic and drive the matching shell call.
/// A projection error or an unknown / undecodable topic is a failure that stalls
/// that topic's ack prefix; the entry re-delivers on the next drain.
fn deliver(projection: &ProjectionShell, entry: &OutboxEntry) -> Result<(), String> {
    if entry.topic == Topic::ViewDocument {
        let view: ViewDocument = from_bytes(&entry.payload).map_err(|e| e.to_string())?;
        projection.reconcile_view(&view).map_err(|e| e.to_string())
    } else if entry.topic == Topic::LandingReceipt {
        let receipt: ProjectedReceipt = from_bytes(&entry.payload).map_err(|e| e.to_string())?;
        projection.project_receipt(&receipt).map_err(|e| e.to_string())
    } else {
        Err(format!("unknown outbox topic {:?}", entry.topic))
    }
}

/// Project a drained batch and compute the per-topic ack. Each entry is
/// reconciled / projected in sequence order; a topic's ack advances through its
/// highest **contiguously delivered** sequence and stops at the first failure,
/// so a stalled entry (undecodable, unknown topic, or a GitHub error) is left
/// undelivered to re-drive rather than acked past. This is the network side of
/// [`MirrorReactorCapability::on_drain_result`], factored out so it is unit-
/// testable against a fake-GitHub-backed shell without the mail harness.
fn project_batch(projection: &ProjectionShell, entries: &[OutboxEntry]) -> Vec<AckOutbox> {
    let mut delivered: BTreeMap<String, u64> = BTreeMap::new();
    let mut stalled: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        if stalled.contains(&entry.topic) {
            continue;
        }
        match deliver(projection, entry) {
            Ok(()) => {
                delivered.insert(entry.topic.clone(), entry.sequence);
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::mirror",
                    topic = %entry.topic,
                    sequence = entry.sequence,
                    %error,
                    "outbox entry not delivered; leaving it undelivered to re-drive",
                );
                stalled.insert(entry.topic.clone());
            }
        }
    }
    delivered.into_iter().map(|(topic, through_sequence)| AckOutbox { topic: Some(topic), through_sequence }).collect()
}

/// Project commission replicas and persist newly created issue numbers.
/// A stall stops this topic's ack prefix and leaves later entries queued,
/// independently of receipts and source mirroring.
///
/// Create is single-flight per commission inside this batch: the store
/// row (overlaid onto the drained payload) is the authority, and a number
/// minted here is recorded before the next sibling is projected so a
/// lagging `find_issue` cannot open a second replica.
fn project_commission_batch(
    projection: &ProjectionShell,
    entries: &[OutboxEntry],
) -> (Vec<AckOutbox>, Vec<RecordCommissionProjection>) {
    let mut through = None;
    let mut persists = Vec::new();
    let mut recorded = BTreeMap::new();
    for entry in entries {
        match deliver_commission(projection, entry, &recorded) {
            Ok(persist) => {
                through = Some(entry.sequence);
                if let Some(persist) = persist {
                    recorded.insert(persist.id.clone(), persist.issue_number);
                    persists.push(persist);
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::mirror",
                    topic = %entry.topic,
                    sequence = entry.sequence,
                    %error,
                    "commission projection not delivered; leaving it undelivered to re-drive",
                );
                break;
            }
        }
    }
    let acks = through
        .map(|through_sequence| AckOutbox { topic: Some(Topic::Commission.as_str().to_owned()), through_sequence })
        .into_iter()
        .collect();
    (acks, persists)
}

fn deliver_commission(
    projection: &ProjectionShell,
    entry: &OutboxEntry,
    recorded: &BTreeMap<String, u64>,
) -> Result<Option<RecordCommissionProjection>, String> {
    let mut document: CommissionProjection = from_bytes(&entry.payload).map_err(|error| error.to_string())?;
    if let Some(&number) = recorded.get(&document.workpiece.0) {
        document.recorded_issue = Some(number);
    }
    let number = projection.project_commission(&document).map_err(|error| error.to_string())?;
    if document.recorded_issue == Some(number) {
        return Ok(None);
    }
    Ok(Some(RecordCommissionProjection { id: document.workpiece.0, issue_number: number }))
}

/// Coalesce superseded replica requests to the latest sequence and push once.
/// A transient failure leaves the whole prefix queued; a rejected force or
/// other deterministic refusal raises an operator-visible alert and still
/// leaves the entry queued so an operator fix can redrive.
fn publish_replica_batch(replica: &SourceReplicaShell, entries: &[OutboxEntry]) -> Vec<AckOutbox> {
    let Some(latest) = entries.iter().rev().find(|entry| entry.topic == Topic::SourceReplica) else {
        return Vec::new();
    };
    match replica.publish() {
        Ok(()) => {
            vec![AckOutbox { topic: Some(Topic::SourceReplica.as_str().to_owned()), through_sequence: latest.sequence }]
        }
        Err(ReplicaError::ForceRejected(detail)) => {
            tracing::error!(
                target: "aether_chassis_bloomery::mirror::alert",
                sequence = latest.sequence,
                error = %detail,
                "source replica force-push was rejected; GitHub was not updated",
            );
            Vec::new()
        }
        Err(ReplicaError::Deterministic(detail)) => {
            tracing::error!(
                target: "aether_chassis_bloomery::mirror::alert",
                sequence = latest.sequence,
                error = %detail,
                "source replica push was refused; GitHub was not updated",
            );
            Vec::new()
        }
        Err(ReplicaError::Transient(detail)) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::mirror",
                topic = %latest.topic,
                sequence = latest.sequence,
                error = %detail,
                "source replica push failed; leaving it undelivered to re-drive",
            );
            Vec::new()
        }
    }
}

#[runtime]
impl NativeActor for MirrorReactorCapability {
    type State = MirrorReactorState;
    type Config = ();
    type Params = MirrorReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.mirror";

    fn init((): (), config: MirrorReactorSetup, ctx: &mut NativeInitCtx<'_>) -> Result<MirrorReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();

        // Unconfigured → disabled: no shells, no timer. The outbox accumulates
        // and republishes once a token/owner/repo is supplied, unless the
        // `fake` backend is selected (#4732) which mounts with an in-memory
        // double and needs no token.
        if config.projection.is_none() && config.replica.is_none() {
            tracing::info!(
                target: "aether_chassis_bloomery::mirror",
                "mirror reactor mounted disabled (unconfigured token/owner/repo); outbox will accumulate",
            );
            return Ok(MirrorReactorState {
                projection: None,
                _source: None,
                replica: None,
                mailer,
                self_mailbox,
                _timer: None,
                awaited_drains: 0,
                outstanding: Arc::default(),
            });
        }

        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            DrainTick::ID,
            DrainTick::default().encode_into_bytes(),
            "aether-bloomery-mirror-drain",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::mirror",
            repository = ?config.repository,
            poll_interval_secs = config.poll_interval_secs,
            source_replica = config.replica.is_some(),
            "mirror reactor mounted; polling the store outbox for projection and source-replica topics",
        );
        Ok(MirrorReactorState {
            projection: config.projection,
            _source: config.source,
            replica: config.replica,
            mailer,
            self_mailbox,
            _timer: Some(timer),
            awaited_drains: 0,
            outstanding: Arc::default(),
        })
    }

    /// Fire the immediate boot-republish tick: the first drain pass republishes
    /// everything left undelivered by a prior crash, so recovery does not wait a
    /// full poll interval. Disabled reactors push nothing.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if state.projection.is_some() || state.replica.is_some() {
            state.mailer.push(Mail::new(
                state.self_mailbox,
                DrainTick::ID,
                DrainTick::default().encode_into_bytes(),
                1,
            ));
        }
    }

    /// Poll wake: drain each owned projection topic. Topic-scoped so this
    /// reactor never touches another's rows; the reply lands at
    /// [`Self::on_drain_result`].
    #[handler::single]
    fn on_drain_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: DrainTick) {
        if state.projection.is_none() && state.replica.is_none() {
            return;
        }
        // One cycle at a time. The drain is non-destructive — an entry stays
        // undelivered until its projection acks — so a tick that fired while
        // the previous cycle was still out would re-drain the same entry into a
        // second worker. The mirror's idempotency is find-then-create against a
        // remote, which converges only when the writes are serial: two workers
        // racing over one entry both read "absent" and both create.
        if state.cycle_in_flight() {
            return;
        }
        let mut drains = Vec::new();
        if state.projection.is_some() {
            drains.push(DrainOutbox::scoped(Topic::ViewDocument));
            drains.push(DrainOutbox::scoped(Topic::LandingReceipt));
            drains.push(DrainOutbox::scoped(Topic::Commission));
        }
        if state.replica.is_some() {
            drains.push(DrainOutbox::scoped(Topic::SourceReplica));
        }
        state.awaited_drains = drains.len();
        for drain in drains {
            ctx.send::<StoreCapability, DrainOutbox>(&drain);
        }
    }

    /// A topic-scoped drain's reply: reconcile / project each undelivered entry,
    /// then ack the contiguous delivered prefix per topic. The GitHub calls are
    /// network I/O, so they run on a detached worker rather than inline on the
    /// dispatcher; the worker acks **on success only**, so a failed or
    /// undecodable entry re-delivers on the next drain.
    #[handler::single]
    fn on_drain_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: DrainOutboxResult) {
        // This reply settles one of the drains the cycle sent, whatever it
        // carries — so the count comes down before any early return, or a topic
        // that drains empty would leave the cycle owed a reply forever.
        state.awaited_drains = state.awaited_drains.saturating_sub(1);

        let projection = state.projection.clone();
        let replica = state.replica.clone();
        if projection.is_none() && replica.is_none() {
            return;
        }
        let entries = match mail {
            DrainOutboxResult::Ok { entries } if !entries.is_empty() => entries,
            DrainOutboxResult::Ok { .. } => return,
            DrainOutboxResult::Err { error } => {
                tracing::warn!(target: "aether_chassis_bloomery::mirror", %error, "outbox drain failed");
                return;
            }
        };

        let slot = WorkerSlot::claim(&state.outstanding);
        ctx.spawn_detached::<MirrorReactorCapability, _>(move |mut root| {
            let _slot = slot; // released here, so the next tick can drain again.
            let (acks, persists) = if entries.first().is_some_and(|entry| entry.topic == Topic::SourceReplica) {
                (replica.as_ref().map_or_else(Vec::new, |replica| publish_replica_batch(replica, &entries)), Vec::new())
            } else if entries.first().is_some_and(|entry| entry.topic == Topic::Commission) {
                projection.as_ref().map_or_else(
                    || (Vec::new(), Vec::new()),
                    |projection| project_commission_batch(projection, &entries),
                )
            } else {
                (
                    projection.as_ref().map_or_else(Vec::new, |projection| project_batch(projection, &entries)),
                    Vec::new(),
                )
            };
            for persist in persists {
                root.send::<StoreCapability, RecordCommissionProjection>(&persist);
            }
            for ack in acks {
                root.send::<StoreCapability, AckOutbox>(&ack);
            }
        });
    }

    /// The store's ack acknowledgement. Nothing to do — the outbox row is
    /// durably delivered; a failure only means the entry re-drives next drain.
    #[handler::single]
    fn on_ack_result(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AckOutboxResult) {
        if let AckOutboxResult::Err { error } = mail {
            tracing::warn!(target: "aether_chassis_bloomery::mirror", %error, "outbox ack failed; entries will re-drive");
        }
    }

    /// Persist of a newly created replica-issue number. A failure leaves the
    /// next enqueue without a recorded number; find-by-marker recovers.
    #[handler::single]
    fn on_record_commission_projection_result(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RecordCommissionProjectionResult,
    ) {
        match mail {
            RecordCommissionProjectionResult::Ok { .. } => {}
            RecordCommissionProjectionResult::Missing { id } => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::mirror",
                    commission = %id,
                    "commission replica-issue number was not recorded; the next projection finds by marker"
                );
            }
            RecordCommissionProjectionResult::Err { error } => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::mirror",
                    %error,
                    "commission replica-issue number was not recorded; the next projection finds by marker"
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! The drain → project → ack core plus the boot-time republish, over a real
    //! `SqliteStore` and a fake-GitHub-backed `ProjectionShell` — the network
    //! side the running capability drives, without the mail harness. `init` /
    //! the timer / `spawn_detached` are the thin glue the chassis-boot test and
    //! compilation cover; this pins the behavior that actually mirrors and acks.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use aether_bloomery::testing::digest;
    use aether_bloomery::{
        BloomDraft, BloomId, ConfigRegistry, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, LandingReceipt,
        Membership, ProjectedReceipt, ResolvedConfigs, Snapshot, SourceReplicaPayload, Topic, WorkpieceId, reduce,
        view_of,
    };
    use aether_bloomery_github::{GithubProjection, ReplicaError, SourceReplica, testing::FakeGithub};
    use aether_data::wire::{from_bytes, to_vec};
    use aether_data::{MailId, MailboxId, Source};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::testing::test_mailer_and_rx;
    use serde::de::DeserializeOwned;

    use super::{
        AckOutbox, DrainOutbox, DrainOutboxResult, DrainTick, Kind, MirrorReactorCapability, MirrorReactorState,
        OutboxEntry, ProjectionShell, SourceReplicaShell, project_batch, project_commission_batch,
        publish_replica_batch,
    };
    use crate::bloomery::outbox::TopicOutbox;
    use crate::store::{SqliteStore, StoreBackend};

    /// Drain egress collecting the payloads of every `UnresolvedMail` carrying
    /// kind `K`, until `want` of them have arrived (each cross-cap send bubbles
    /// to the loopback outbound as an `UnresolvedMail` because the peer mailbox
    /// is unregistered under `new_for_test`).
    fn collect_sends<K: Kind + DeserializeOwned>(rx: &Receiver<EgressEvent>, want: usize) -> Vec<K> {
        let mut got = Vec::new();
        while got.len() < want {
            let event = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("test: the expected cross-cap send arrives within the deadline");
            if let EgressEvent::UnresolvedMail { kind_id, payload, .. } = event
                && kind_id == K::ID
            {
                got.push(from_bytes::<K>(&payload).expect("test: send payload decodes"));
            }
        }
        got
    }

    /// The issue the encoded view's sole member addresses — an object the
    /// repository already holds, which a test seeds before projecting.
    const MEMBER_ISSUE: u64 = 4628;

    /// A sealed single-member bloom's view document, encoded as the outbox
    /// payload the producer enqueues (the real journal-then-reduce-then-view
    /// path, not a hand-built value). One member → one folded comment on the
    /// issue it addresses when reconciled.
    fn encoded_view() -> Vec<u8> {
        let scope_revision = digest(10);
        let mut member = Membership {
            workpiece: WorkpieceId(format!("issue-{MEMBER_ISSUE}")),
            scope_revision,
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        // The approval binds the member's whole subject (ADR-0174), which is only
        // computable once the rest of the member is built.
        member.approval.subject = member.subject();
        let base = digest(0);
        // The seal-time catalog admission (ADR-0149 §The line) rejects the zero
        // default, so the draft must promise the one line the pipeline runs.
        let spec = BloomDraft { proposals: vec![member], base, ..BloomDraft::default() }.seal();
        let event = Event { idempotency_key: IdempotencyKey("seal-1".into()), fact: Fact::Seal(spec) };

        let mut snapshot = Snapshot::new(base);
        snapshot = snapshot.apply(
            &event,
            &reduce(&snapshot, &event, &ResolvedConfigs::default(), &aether_bloomery::SpendWindow::default()),
            &ResolvedConfigs::default(),
        );
        to_vec(&view_of(&snapshot, |_| None)).unwrap()
    }

    #[test]
    fn drains_projects_and_acks_its_topic_then_republishes_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("mirror.db");
        let db = db.to_str().unwrap();

        let fake = FakeGithub::new();
        fake.seed_issue(MEMBER_ISSUE, "the workpiece's own issue");
        let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

        // Phase 1 — steady projection: enqueue a view document, drain its topic,
        // project it, and ack the delivered prefix.
        {
            let mut store = SqliteStore::open(db).unwrap();
            store.enqueue_topic(Topic::ViewDocument, &encoded_view()).unwrap();

            let entries = store.drain_topic(Topic::ViewDocument).unwrap();
            assert_eq!(entries.len(), 1, "the enqueued view is drainable on its topic");

            let acks = project_batch(&shell, &entries);
            assert_eq!(fake.issue_count(), 1, "the mirror opens no object of its own");
            assert_eq!(fake.comments_on(MEMBER_ISSUE).len(), 1, "the member's folded comment on the issue it names");
            assert_eq!(acks.len(), 1);
            assert!(
                acks[0].topic.as_deref().is_some_and(|topic| topic == Topic::ViewDocument),
                "the ack covers the view-document topic",
            );
            assert_eq!(acks[0].through_sequence, entries[0].sequence, "ack covers the delivered entry");

            // On success only: apply the ack the worker would send.
            store.ack_outbox(acks[0].topic.as_deref(), acks[0].through_sequence).unwrap();
            assert!(store.drain_topic(Topic::ViewDocument).unwrap().is_empty(), "the acked entry does not re-drain");
        }

        // Phase 2 — boot republish: a second view is enqueued but the process
        // crashes before acking it. A fresh reactor against the same store
        // re-drains the still-undelivered entry on its first pass and delivers
        // it (the boot-republish Done that moved here from #3497). Reconcile is
        // idempotent, so the re-projection converges to the same carbon copy.
        {
            let mut store = SqliteStore::open(db).unwrap();
            store.enqueue_topic(Topic::ViewDocument, &encoded_view()).unwrap();
            drop(store);

            // Simulated restart: reopen the same database file.
            let mut restarted = SqliteStore::open(db).unwrap();
            let republished = restarted.drain_topic(Topic::ViewDocument).unwrap();
            assert_eq!(republished.len(), 1, "the unacked entry survived the restart and re-drains");

            let acks = project_batch(&shell, &republished);
            assert_eq!(fake.comments_on(MEMBER_ISSUE).len(), 1, "idempotent reconcile converges to the same comment");
            assert_eq!(acks.len(), 1);

            restarted.ack_outbox(acks[0].topic.as_deref(), acks[0].through_sequence).unwrap();
            assert!(restarted.drain_topic(Topic::ViewDocument).unwrap().is_empty(), "republished entry now acked");
        }
    }
    #[test]
    fn a_landing_receipt_projects_a_comment_on_its_topic() {
        // ADR-0149 migration step 3: a gate-enabled land emits a receipt the
        // control actor enqueues under the receipt topic, which the mirror
        // reactor drains and projects as a comment on each landed member's own
        // issue. This pins the receipt path — that the reactor topic matches the
        // producer's (the mismatch step 3 reconciled), and that the topic's
        // payload is the membership-carrying envelope rather than the bare
        // receipt, which would decode into a projection that reaches nothing.
        let fake = FakeGithub::new();
        fake.seed_issue(MEMBER_ISSUE, "the workpiece's own issue");
        let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));
        let mut store = SqliteStore::open(":memory:").unwrap();

        let projected = ProjectedReceipt {
            receipt: LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(10), new_head: digest(20) },
            members: vec![WorkpieceId(format!("issue-{MEMBER_ISSUE}"))],
        };
        store.enqueue_topic(Topic::LandingReceipt, &to_vec(&projected).unwrap()).unwrap();

        let entries = store.drain_topic(Topic::LandingReceipt).unwrap();
        assert_eq!(entries.len(), 1, "the enqueued receipt is drainable on the receipt topic");

        let acks = project_batch(&shell, &entries);
        assert_eq!(fake.comments_on(MEMBER_ISSUE).len(), 1, "the receipt lands on the member's own issue");
        assert_eq!(fake.issue_count(), 1, "a receipt opens nothing");
        assert_eq!(acks.len(), 1);
        assert!(
            acks[0].topic.as_deref().is_some_and(|topic| topic == Topic::LandingReceipt),
            "the ack covers the receipt topic",
        );
        assert_eq!(acks[0].through_sequence, entries[0].sequence);
    }

    #[test]
    fn mail_driven_drain_projects_and_acks_through_the_actor_path() {
        // Drive the capability's handlers through the substrate cap-test fixture
        // — the actual actor/mail path (`on_drain_tick` → topic-scoped
        // `DrainOutbox` sends, then `on_drain_result` → the `spawn_detached`
        // worker → `project_batch` → `AckOutbox` send), over a fake-GitHub-backed
        // shell. Cross-cap sends to the unregistered `aether.store` mailbox bubble
        // to the loopback outbound, so the test reads them off egress.
        let (mailer, rx) = test_mailer_and_rx();
        let self_mailbox = MailboxId(0);
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), self_mailbox));

        let fake = FakeGithub::new();
        fake.seed_issue(MEMBER_ISSUE, "the workpiece's own issue");
        let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));
        let mut state = MirrorReactorState::with_shells(Some(shell), None, Arc::clone(&mailer), self_mailbox);

        // on_drain_tick fans one topic-scoped drain per owned projection topic.
        {
            let mut ctx = NativeCtx::new_dispatching(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            MirrorReactorCapability::on_drain_tick(&mut state, ctx.as_single(), DrainTick::default());
        }
        binding.flush_outbound();
        let mut drained_topics: Vec<Option<String>> =
            collect_sends::<DrainOutbox>(&rx, 3).into_iter().map(|d| d.topic).collect();
        drained_topics.sort();
        let mut expected = vec![
            DrainOutbox::scoped(Topic::LandingReceipt).topic,
            DrainOutbox::scoped(Topic::ViewDocument).topic,
            DrainOutbox::scoped(Topic::Commission).topic,
        ];
        expected.sort();
        assert_eq!(drained_topics, expected, "each owned projection topic is drained, scoped by topic");

        // on_drain_result projects the entry and — on success — the detached
        // worker acks the delivered prefix. The ack landing on egress proves the
        // worker ran (it sends the ack only after the reconcile returns Ok).
        let entry =
            OutboxEntry { sequence: 7, topic: Topic::ViewDocument.as_str().to_owned(), payload: encoded_view() };
        {
            let mut ctx = NativeCtx::new_dispatching(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            MirrorReactorCapability::on_drain_result(
                &mut state,
                ctx.as_single(),
                DrainOutboxResult::Ok { entries: vec![entry] },
            );
        }
        let acks = collect_sends::<AckOutbox>(&rx, 1);
        assert_eq!(fake.comments_on(MEMBER_ISSUE).len(), 1, "the worker reconciled the mirror before acking");
        assert!(
            acks[0].topic.as_deref().is_some_and(|topic| topic == Topic::ViewDocument),
            "the ack covers the view-document topic",
        );
        assert_eq!(acks[0].through_sequence, 7, "the ack covers the delivered entry's sequence");
    }

    #[test]
    fn a_tick_during_a_running_cycle_does_not_re_drain_the_same_entry() {
        // The incident this guards: one landing receipt projected seven times
        // over. A projection slower than the poll interval left its entry
        // undelivered, every tick in that window re-drained it, and the
        // overlapping workers each read "absent" from find-then-create and each
        // wrote its own copy. A cycle must run alone — and must release, or the
        // reactor wedges at permanently-busy and nothing ever mirrors.
        let (mailer, rx) = test_mailer_and_rx();
        let self_mailbox = MailboxId(0);
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), self_mailbox));
        let shell = ProjectionShell::new(Arc::new(GithubProjection::new(FakeGithub::new())));
        let mut state = MirrorReactorState::with_shells(Some(shell), None, Arc::clone(&mailer), self_mailbox);

        let tick = |state: &mut MirrorReactorState| {
            let mut ctx = NativeCtx::new_dispatching(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            MirrorReactorCapability::on_drain_tick(state, ctx.as_single(), DrainTick::default());
            binding.flush_outbound();
        };
        let settle_one_drain = |state: &mut MirrorReactorState| {
            let mut ctx = NativeCtx::new_dispatching(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            MirrorReactorCapability::on_drain_result(
                state,
                ctx.as_single(),
                DrainOutboxResult::Ok { entries: Vec::new() },
            );
        };
        // Read as "nothing was sent": the channel is drained to empty by the
        // preceding assertions, so a leaked send is the only thing that could
        // arrive. Asserting emptiness is what makes the skip observable — a
        // later `collect_sends` would happily consume a leaked pair and pass.
        let assert_silent = |what: &str| {
            assert!(
                rx.recv_timeout(Duration::from_millis(200)).is_err(),
                "test: {what} must send nothing while a cycle is in flight",
            );
        };

        // The opening tick drains both owned topics.
        tick(&mut state);
        assert_eq!(collect_sends::<DrainOutbox>(&rx, 3).len(), 3, "the opening tick drains every projection topic");

        // A tick arriving while those drains are still owed replies must send
        // nothing — this is the window that re-drove the undelivered receipt.
        tick(&mut state);
        assert_silent("a tick with every drain still owed replies");

        settle_one_drain(&mut state);
        tick(&mut state);
        assert_silent("a tick with drains still owed a reply");

        settle_one_drain(&mut state);
        tick(&mut state);
        assert_silent("a tick with one drain still owed a reply");

        // Every drain has now settled, so the cycle is complete and released.
        settle_one_drain(&mut state);
        tick(&mut state);
        let mut topics: Vec<Option<String>> =
            collect_sends::<DrainOutbox>(&rx, 3).into_iter().map(|d| d.topic).collect();
        topics.sort();
        let mut expected = vec![
            DrainOutbox::scoped(Topic::LandingReceipt).topic,
            DrainOutbox::scoped(Topic::ViewDocument).topic,
            DrainOutbox::scoped(Topic::Commission).topic,
        ];
        expected.sort();
        assert_eq!(topics, expected, "a completed cycle releases, so the next tick drains every topic afresh");
    }

    struct FakeReplica {
        fail: bool,
        reject_force: bool,
        refuse: bool,
        publishes: AtomicUsize,
        alerts: Mutex<Vec<String>>,
    }

    impl FakeReplica {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                fail: false,
                reject_force: false,
                refuse: false,
                publishes: AtomicUsize::new(0),
                alerts: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                fail: true,
                reject_force: false,
                refuse: false,
                publishes: AtomicUsize::new(0),
                alerts: Mutex::new(Vec::new()),
            })
        }

        fn rejecting() -> Arc<Self> {
            Arc::new(Self {
                fail: false,
                reject_force: true,
                refuse: false,
                publishes: AtomicUsize::new(0),
                alerts: Mutex::new(Vec::new()),
            })
        }

        fn refusing() -> Arc<Self> {
            Arc::new(Self {
                fail: false,
                reject_force: false,
                refuse: true,
                publishes: AtomicUsize::new(0),
                alerts: Mutex::new(Vec::new()),
            })
        }

        fn count(&self) -> usize {
            self.publishes.load(Ordering::SeqCst)
        }
    }

    impl SourceReplica for FakeReplica {
        fn publish(&self) -> Result<(), ReplicaError> {
            self.publishes.fetch_add(1, Ordering::SeqCst);
            if self.reject_force {
                let detail = "protected branch hook declined".to_owned();
                self.alerts.lock().unwrap().push(detail.clone());
                return Err(ReplicaError::ForceRejected(detail));
            }
            if self.refuse {
                let detail = "invalid credentials".to_owned();
                self.alerts.lock().unwrap().push(detail.clone());
                return Err(ReplicaError::Deterministic(detail));
            }
            if self.fail {
                return Err(ReplicaError::Transient("github unreachable".into()));
            }
            Ok(())
        }
    }

    fn replica_entry(sequence: u64) -> OutboxEntry {
        OutboxEntry {
            sequence,
            topic: Topic::SourceReplica.as_str().to_owned(),
            payload: to_vec(&SourceReplicaPayload { new_head: digest(20) }).unwrap(),
        }
    }

    #[test]
    fn a_failing_replica_leaves_the_land_final_and_the_entry_queued() {
        // The land is already admitted (the replica topic is host-minted after
        // that). A push fault must not ack the replica row and must not invent
        // a second land.
        let fake = FakeReplica::failing();
        let shell = SourceReplicaShell::new(fake.clone());
        let mut store = SqliteStore::open(":memory:").unwrap();
        store.enqueue_topic(Topic::SourceReplica, &replica_entry(1).payload).unwrap();

        let entries = store.drain_topic(Topic::SourceReplica).unwrap();
        let acks = publish_replica_batch(&shell, &entries);
        assert!(acks.is_empty(), "a failed push is not acked");
        assert_eq!(fake.count(), 1);
        assert_eq!(store.drain_topic(Topic::SourceReplica).unwrap().len(), 1, "the entry stays queued for redrive");
        assert!(store.drain_topic(Topic::LandingReceipt).unwrap().is_empty(), "the land receipt topic is untouched");
    }

    #[test]
    fn a_rejected_force_surfaces_an_operator_visible_alert() {
        let fake = FakeReplica::rejecting();
        let shell = SourceReplicaShell::new(fake.clone());
        let acks = publish_replica_batch(&shell, &[replica_entry(3)]);
        assert!(acks.is_empty(), "a rejected force is not silently acked away");
        assert_eq!(
            fake.alerts.lock().unwrap().as_slice(),
            ["protected branch hook declined"],
            "the rejected force is raised as an operator-visible alert rather than retried silently",
        );
    }

    #[test]
    fn a_deterministic_refusal_surfaces_an_operator_visible_alert() {
        let fake = FakeReplica::refusing();
        let shell = SourceReplicaShell::new(fake.clone());
        let acks = publish_replica_batch(&shell, &[replica_entry(3)]);
        assert!(acks.is_empty(), "a deterministic refusal is not silently acked away");
        assert_eq!(
            fake.alerts.lock().unwrap().as_slice(),
            ["invalid credentials"],
            "a deterministic refusal is raised as an operator-visible alert rather than retried silently",
        );
    }

    #[test]
    fn superseded_replica_requests_coalesce_to_the_latest_head() {
        let fake = FakeReplica::ok();
        let shell = SourceReplicaShell::new(fake.clone());
        let entries = vec![replica_entry(1), replica_entry(2), replica_entry(3)];
        let acks = publish_replica_batch(&shell, &entries);
        assert_eq!(fake.count(), 1, "an outage must not replay every intermediate head");
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].through_sequence, 3, "success acks through the latest sequence");
    }

    #[test]
    fn a_stalled_projection_does_not_head_of_line_block_the_source_replica() {
        // Independent topics and ack prefixes: a GitHub issue projection
        // failure must not stall source refs, and a replica failure must not
        // stall the view.
        let fake = FakeGithub::new();
        fake.seed_issue(MEMBER_ISSUE, "the workpiece's own issue");
        let projection = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));
        let replica = FakeReplica::ok();
        let replica_shell = SourceReplicaShell::new(replica.clone());

        let bad_view = OutboxEntry {
            sequence: 1,
            topic: Topic::ViewDocument.as_str().to_owned(),
            payload: b"not-a-view".to_vec(),
        };
        let view_acks = project_batch(&projection, &[bad_view]);
        assert!(view_acks.is_empty(), "an undecodable view stalls its own topic");

        let replica_acks = publish_replica_batch(&replica_shell, &[replica_entry(2)]);
        assert_eq!(replica_acks.len(), 1, "the replica topic still delivers");
        assert_eq!(replica.count(), 1);

        let replica = FakeReplica::failing();
        let replica_shell = SourceReplicaShell::new(replica);
        let replica_acks = publish_replica_batch(&replica_shell, &[replica_entry(4)]);
        assert!(replica_acks.is_empty(), "a replica fault stalls only the replica topic");

        let view = OutboxEntry { sequence: 5, topic: Topic::ViewDocument.as_str().to_owned(), payload: encoded_view() };
        let view_acks = project_batch(&projection, &[view]);
        assert_eq!(view_acks.len(), 1, "the view topic still delivers beside a stalled replica");
        assert_eq!(fake.comments_on(MEMBER_ISSUE).len(), 1);
    }

    fn commission_entry(sequence: u64, workpiece: &str, recorded_issue: Option<u64>) -> OutboxEntry {
        OutboxEntry {
            sequence,
            topic: Topic::Commission.as_str().to_owned(),
            payload: to_vec(&aether_bloomery::CommissionProjection {
                workpiece: WorkpieceId(workpiece.to_owned()),
                intent: digest(1),
                scope_revision: Some(digest(2)),
                approval_signer: None,
                approval_digest: None,
                status: "open".to_owned(),
                recorded_issue,
            })
            .unwrap(),
        }
    }

    #[test]
    fn two_commission_entries_for_one_workpiece_create_once_then_update() {
        // Pre-fix: both payloads freeze recorded_issue=None, and find_issue
        // lags the first create, so the second entry opens a sibling. The
        // batch must record the created number before the next project so
        // the second write is an update.
        let fake = FakeGithub::new();
        fake.lag_next_find_after_create();
        let projection = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

        let first = commission_entry(1, "wp-1", None);
        let second = {
            let mut entry = commission_entry(2, "wp-1", None);
            let mut document: aether_bloomery::CommissionProjection = from_bytes(&entry.payload).unwrap();
            document.approval_signer = Some("operator".to_owned());
            document.approval_digest = Some(digest(3));
            entry.payload = to_vec(&document).unwrap();
            entry
        };

        let (acks, persists) = project_commission_batch(&projection, &[first, second]);

        assert_eq!(fake.created_issue_count(), 1, "the first entry creates the replica");
        assert_eq!(fake.updated_issue_count(), 1, "the second entry updates that replica");
        assert_eq!(fake.issue_count(), 1, "one commission owns one issue");
        assert_eq!(persists.len(), 1, "only the create is persisted; the update already has the number");
        assert_eq!(persists[0].id, "wp-1");
        assert_eq!(persists[0].issue_number, fake.issue_numbers()[0]);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].through_sequence, 2, "both entries deliver on the same topic prefix");
        let title = fake.issue_title(fake.issue_numbers()[0]).expect("replica");
        assert!(
            title.starts_with("Bloomery replica"),
            "the replica title must not collide with human issue numbering: {title}"
        );
    }

    #[test]
    fn a_stalled_commission_topic_does_not_stall_receipts_or_mirroring() {
        // Independent topics and ack prefixes: one bad replica issue must not
        // hold receipts or the view document, and a view stall must not hold
        // a healthy commission projection.
        let fake = FakeGithub::new();
        fake.seed_issue(MEMBER_ISSUE, "the workpiece's own issue");
        let projection = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

        let bad_commission = OutboxEntry {
            sequence: 1,
            topic: Topic::Commission.as_str().to_owned(),
            payload: b"not-a-commission".to_vec(),
        };
        let (commission_acks, persists) = project_commission_batch(&projection, &[bad_commission]);
        assert!(commission_acks.is_empty(), "an undecodable commission stalls its own topic");
        assert!(persists.is_empty());

        let receipt = OutboxEntry {
            sequence: 2,
            topic: Topic::LandingReceipt.as_str().to_owned(),
            payload: to_vec(&ProjectedReceipt {
                receipt: LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(10), new_head: digest(20) },
                members: vec![WorkpieceId(format!("issue-{MEMBER_ISSUE}"))],
            })
            .unwrap(),
        };
        let receipt_acks = project_batch(&projection, &[receipt]);
        assert_eq!(receipt_acks.len(), 1, "the receipt topic still delivers");

        let view = OutboxEntry { sequence: 3, topic: Topic::ViewDocument.as_str().to_owned(), payload: encoded_view() };
        let view_acks = project_batch(&projection, &[view]);
        assert_eq!(view_acks.len(), 1, "the view topic still delivers beside a stalled commission");
        assert_eq!(fake.comments_on(MEMBER_ISSUE).len(), 2, "receipt and view both wrote");
    }
}
