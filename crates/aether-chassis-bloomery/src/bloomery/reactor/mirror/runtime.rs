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
use aether_bloomery::{LandingReceipt, Topic, ViewDocument};
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::MirrorReactorCapability;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::{GithubMirrorConfig, ProjectionShell, SourceShell};
use crate::store::{AckOutbox, AckOutboxResult, DrainOutbox, DrainOutboxResult, OutboxEntry, StoreCapability};

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
        Self {
            projection,
            _source: source,
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
        let receipt: LandingReceipt = from_bytes(&entry.payload).map_err(|e| e.to_string())?;
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

#[runtime]
impl NativeActor for MirrorReactorCapability {
    type State = MirrorReactorState;
    type Config = GithubMirrorConfig;

    const NAMESPACE: &'static str = "aether.bloomery.mirror";

    fn init(config: GithubMirrorConfig, ctx: &mut NativeInitCtx<'_>) -> Result<MirrorReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();

        // Unconfigured → disabled: no shells, no timer. The outbox accumulates
        // and republishes once a token/owner/repo is supplied, unless the
        // `fake` backend is selected (#4732) which mounts with an in-memory
        // double and needs no token.
        let configured =
            config.uses_fixture() || !(config.token.is_empty() || config.owner.is_empty() || config.repo.is_empty());
        if !configured {
            tracing::info!(
                target: "aether_chassis_bloomery::mirror",
                "mirror reactor mounted disabled (unconfigured token/owner/repo); outbox will accumulate",
            );
            return Ok(MirrorReactorState {
                projection: None,
                _source: None,
                mailer,
                self_mailbox,
                _timer: None,
                awaited_drains: 0,
                outstanding: Arc::default(),
            });
        }

        #[cfg(any(test, feature = "testing"))]
        let (projection, source) = if config.uses_fixture() {
            let fake = config.shared_fixture();
            let fake_for_projection = fake.clone();
            let projection =
                ProjectionShell::new(Arc::new(aether_bloomery_github::GithubProjection::new(fake_for_projection)));
            let source = {
                use aether_bloomery_github::{GitSource, SharedCorrespondence};
                use std::sync::Arc;
                let fake_for_git = fake.clone();
                let correspondence: SharedCorrespondence = Arc::new(fake);
                let git_source = GitSource::new(fake_for_git, Arc::clone(&correspondence), config.cas_land_enabled);
                SourceShell::new_with_correspondence(Arc::new(git_source), Arc::clone(&correspondence))
            };
            (projection, source)
        } else {
            let projection = ProjectionShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
            let source = SourceShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
            (projection, source)
        };
        #[cfg(not(any(test, feature = "testing")))]
        let (projection, source) = {
            let projection = ProjectionShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
            let source = SourceShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
            (projection, source)
        };
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
            owner = %config.owner,
            repo = %config.repo,
            poll_interval_secs = config.poll_interval_secs,
            "mirror reactor mounted; polling the store outbox for projection topics",
        );
        Ok(MirrorReactorState {
            projection: Some(projection),
            _source: Some(source),
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
        if state.projection.is_some() {
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
        if state.projection.is_none() {
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
        let drains = [DrainOutbox::scoped(Topic::ViewDocument), DrainOutbox::scoped(Topic::LandingReceipt)];
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

        let Some(projection) = state.projection.clone() else {
            return;
        };
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
            for ack in project_batch(&projection, &entries) {
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! The drain → project → ack core plus the boot-time republish, over a real
    //! `SqliteStore` and a fake-GitHub-backed `ProjectionShell` — the network
    //! side the running capability drives, without the mail harness. `init` /
    //! the timer / `spawn_detached` are the thin glue the chassis-boot test and
    //! compilation cover; this pins the behavior that actually mirrors and acks.

    use std::sync::Arc;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use aether_bloomery::{
        BloomDraft, BloomId, ConfigRegistry, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey,
        LandingReceipt, Membership, ResolvedConfigs, Snapshot, Topic, WorkpieceId, reduce, view_of,
    };
    use aether_bloomery_github::{GithubProjection, testing::FakeGithub};
    use aether_data::wire::{from_bytes, to_vec};
    use aether_data::{MailId, MailboxId, Source};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::testing::test_mailer_and_rx;
    use serde::de::DeserializeOwned;

    use super::{
        AckOutbox, DrainOutbox, DrainOutboxResult, DrainTick, Kind, MirrorReactorCapability, MirrorReactorState,
        OutboxEntry, ProjectionShell, project_batch,
    };
    use crate::bloomery::outbox::TopicOutbox;
    use crate::store::{SqliteStore, StoreBackend};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

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

    /// A sealed single-member bloom's view document, encoded as the outbox
    /// payload the producer enqueues (the real journal-then-reduce-then-view
    /// path, not a hand-built value). One member → one umbrella issue plus one
    /// workpiece issue when reconciled.
    fn encoded_view() -> Vec<u8> {
        let scope_revision = digest(10);
        let mut member = Membership {
            workpiece: WorkpieceId("reactor-core".into()),
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
            &reduce(&snapshot, &event, &ResolvedConfigs::default()),
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
        let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

        // Phase 1 — steady projection: enqueue a view document, drain its topic,
        // project it, and ack the delivered prefix.
        {
            let mut store = SqliteStore::open(db).unwrap();
            store.enqueue_topic(Topic::ViewDocument, &encoded_view()).unwrap();

            let entries = store.drain_topic(Topic::ViewDocument).unwrap();
            assert_eq!(entries.len(), 1, "the enqueued view is drainable on its topic");

            let acks = project_batch(&shell, &entries);
            assert_eq!(fake.issue_count(), 2, "the carbon copy: one umbrella issue plus one workpiece issue");
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
            assert_eq!(fake.issue_count(), 2, "idempotent reconcile converges to the same carbon copy");
            assert_eq!(acks.len(), 1);

            restarted.ack_outbox(acks[0].topic.as_deref(), acks[0].through_sequence).unwrap();
            assert!(restarted.drain_topic(Topic::ViewDocument).unwrap().is_empty(), "republished entry now acked");
        }
    }
    #[test]
    fn a_landing_receipt_projects_a_comment_on_its_topic() {
        // ADR-0149 migration step 3: a gate-enabled land emits a `LandingReceipt`
        // the control actor enqueues under the receipt topic, which the mirror
        // reactor drains and projects as a comment on the bloom's umbrella issue.
        // This pins the receipt path — and that the reactor topic matches the
        // producer's, the mismatch step 3 reconciled (a stranded receipt would
        // drain nothing and project no comment).
        let fake = FakeGithub::new();
        let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));
        let mut store = SqliteStore::open(":memory:").unwrap();

        let receipt = LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(10), new_head: digest(20) };
        store.enqueue_topic(Topic::LandingReceipt, &to_vec(&receipt).unwrap()).unwrap();

        let entries = store.drain_topic(Topic::LandingReceipt).unwrap();
        assert_eq!(entries.len(), 1, "the enqueued receipt is drainable on the receipt topic");

        let acks = project_batch(&shell, &entries);
        assert_eq!(fake.comment_count(), 1, "the receipt projects one landing comment on the umbrella issue");
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
        let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));
        let mut state = MirrorReactorState::with_shells(Some(shell), None, Arc::clone(&mailer), self_mailbox);

        // on_drain_tick fans one topic-scoped drain per owned projection topic.
        {
            let mut ctx = NativeCtx::new_dispatching(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            MirrorReactorCapability::on_drain_tick(&mut state, ctx.as_single(), DrainTick::default());
        }
        binding.flush_outbound();
        let mut drained_topics: Vec<Option<String>> =
            collect_sends::<DrainOutbox>(&rx, 2).into_iter().map(|d| d.topic).collect();
        drained_topics.sort();
        let mut expected =
            vec![DrainOutbox::scoped(Topic::LandingReceipt).topic, DrainOutbox::scoped(Topic::ViewDocument).topic];
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
        assert_eq!(fake.issue_count(), 2, "the worker reconciled the carbon copy before acking");
        assert!(
            acks[0].topic.as_deref().is_some_and(|topic| topic == Topic::ViewDocument),
            "the ack covers the view-document topic",
        );
        assert_eq!(acks[0].through_sequence, 7, "the ack covers the delivered entry's sequence");
    }

    #[test]
    fn a_tick_during_a_running_cycle_does_not_re_drain_the_same_entry() {
        // The incident this guards: one landing receipt opened seven umbrella
        // issues. A projection slower than the poll interval left its entry
        // undelivered, every tick in that window re-drained it, and the
        // overlapping workers each read "absent" from find-then-create and each
        // created its own copy. A cycle must run alone — and must release, or
        // the reactor wedges at permanently-busy and nothing ever mirrors.
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
        assert_eq!(collect_sends::<DrainOutbox>(&rx, 2).len(), 2, "the opening tick drains both projection topics");

        // A tick arriving while those drains are still owed replies must send
        // nothing — this is the window that re-drove the undelivered receipt.
        tick(&mut state);
        assert_silent("a tick with both drains still owed replies");

        settle_one_drain(&mut state);
        tick(&mut state);
        assert_silent("a tick with one drain still owed a reply");

        // Both drains have now settled, so the cycle is complete and released.
        settle_one_drain(&mut state);
        tick(&mut state);
        let mut topics: Vec<Option<String>> =
            collect_sends::<DrainOutbox>(&rx, 2).into_iter().map(|d| d.topic).collect();
        topics.sort();
        let mut expected =
            vec![DrainOutbox::scoped(Topic::LandingReceipt).topic, DrainOutbox::scoped(Topic::ViewDocument).topic];
        expected.sort();
        assert_eq!(topics, expected, "a completed cycle releases, so the next tick drains both topics afresh");
    }
}
