//! The poll-driven doctor pass: rebuild live state, evaluate the seed
//! invariants, mail the completed report to the REST API, and post new
//! violations through the operator alert channel.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_actor::runtime;
use aether_bloomery::{
    Admit, AdmitResult, BackendObjectId, BloomId, BloomStatus, Digest, Event, Evidence, EvidenceKind, Fact,
    IdempotencyKey, Outcome, ResolvedConfigs, SharedCorrespondence, Snapshot, StageId, Topic, WorkpieceId,
    decode_recorded_decisions, decode_recorded_event, is_active_unlanded,
};
use aether_bloomery_github::GitObjectId;
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;

use super::invariants::{
    DoctorReport, KnownFlake, LiveState, OpenDispatch, ReplicaObservation, SurfaceParkObservation, evaluate,
    undispatched_members,
};
use super::{DoctorReactorCapability, DoctorReactorSetup, LatestDoctorReport};
use crate::api::BloomeryApiCapability;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::{ExecutorShell, KNOWN_FLAKE_CANDIDATES, SourceShell};
use crate::control::ControlCore;
use crate::store::{OutboxEntry, SqliteStore, StoreBackend};

/// The self-addressed wake the poll timer fires each interval.
#[aether_data::kind(name = "aether.bloomery.doctor.doctor_tick", default)]
pub struct DoctorTick {}

/// Runtime state for [`DoctorReactorCapability`].
pub struct DoctorReactorState {
    source: Option<SourceShell>,
    executor: Option<ExecutorShell>,
    correspondence: Option<SharedCorrespondence>,
    store: Option<SqliteStore>,
    worktree_base: PathBuf,
    last_fingerprint: String,
    replica_seen: BTreeMap<u64, Instant>,
    replica_passes: BTreeMap<u64, u32>,
    surface_seen: BTreeMap<(BloomId, WorkpieceId), Instant>,
    unresolved_head_seen: Option<(String, Instant)>,
    /// First sighting of the current live daily sha. The head checks read its
    /// age as the lag budget (#6025): a young sha the pointers still miss is
    /// a ref that moved, an old one an observer that stopped.
    observe_head_seen: Option<(String, Instant)>,
    /// Members seen with no live lane and no pending dispatch, by first
    /// sighting. A member still undispatched a full poll interval later is
    /// re-dispatched: the interval is what separates a handoff between two
    /// ticks from a dispatch the host actually lost.
    undispatched_seen: BTreeMap<(BloomId, WorkpieceId), Instant>,
    /// How often this reactor wakes.
    poll_interval: Duration,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    _timer: Option<TimerHandle>,
}

/// Advance the alert fingerprint and report whether this pass is newly loud.
fn alert_and_advance(last_fingerprint: &mut String, report: &DoctorReport) -> bool {
    let fingerprint = report.fingerprint();
    let alert = fingerprint != *last_fingerprint && !report.is_clean();
    *last_fingerprint = fingerprint;
    alert
}

#[runtime]
impl NativeActor for DoctorReactorCapability {
    type State = DoctorReactorState;
    type Config = ();
    type Params = DoctorReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.doctor";

    fn init((): (), config: DoctorReactorSetup, ctx: &mut NativeInitCtx<'_>) -> Result<DoctorReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let store = SqliteStore::open(&config.store_path).map_err(|error| BootError::Other(Box::new(error)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            DoctorTick::ID,
            DoctorTick::default().encode_into_bytes(),
            "aether-bloomery-doctor",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::doctor",
            poll_interval_secs = config.poll_interval_secs,
            "doctor mounted; evaluating cross-source invariants on the coordinator cadence",
        );
        Ok(DoctorReactorState {
            source: config.source,
            executor: config.executor,
            correspondence: config.correspondence,
            store: Some(store),
            worktree_base: PathBuf::from(config.worktree_base),
            last_fingerprint: String::new(),
            replica_seen: BTreeMap::new(),
            replica_passes: BTreeMap::new(),
            surface_seen: BTreeMap::new(),
            unresolved_head_seen: None,
            observe_head_seen: None,
            undispatched_seen: BTreeMap::new(),
            poll_interval: interval,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot pass so a stranded claim ref or drifted head is
    /// loud before the first poll interval, once the journal is readable.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        state.mailer.push(Mail::new(state.self_mailbox, DoctorTick::ID, DoctorTick::default().encode_into_bytes(), 1));
    }

    /// Settle the re-dispatch this reactor admitted. The admission is sent on
    /// the tick's own chain so the fresh dispatch lands before the tick
    /// settles, which means its reply comes back here: a `Duplicate` outcome
    /// says the journal already held this redispatch, and an `Err` says the
    /// member is still standing with nobody told.
    #[handler::single]
    fn on_admit_result(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AdmitResult) {
        match mail {
            AdmitResult::Ok { outcome } => tracing::debug!(
                target: "aether_chassis_bloomery::doctor",
                outcome = ?from_bytes::<Outcome>(&outcome).ok(),
                "doctor redispatch admitted",
            ),
            AdmitResult::Err { error } => tracing::error!(
                target: "aether_chassis_bloomery::doctor::alert",
                %error,
                "doctor redispatch was refused; the member is still standing",
            ),
        }
    }

    #[handler::single]
    fn on_doctor_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: DoctorTick) {
        let executor = state.executor.clone();
        let lanes_running = executor.as_ref().is_some_and(|shell| shell.lane_occupancy().any_running());
        let started_nonces = executor.as_ref().map_or_else(Vec::new, ExecutorShell::started_nonces);
        let now = Instant::now();
        let poll_interval = state.poll_interval;
        let Some(store) = state.store.as_mut() else {
            return;
        };
        match collect_and_evaluate(&mut CollectRequest {
            store,
            source: state.source.as_ref(),
            correspondence: state.correspondence.as_ref(),
            worktree_base: &state.worktree_base,
            lanes_running,
            started_nonces: &started_nonces,
            replica_seen: &mut state.replica_seen,
            replica_passes: &mut state.replica_passes,
            surface_seen: &mut state.surface_seen,
            unresolved_head_seen: &mut state.unresolved_head_seen,
            observe_head_seen: &mut state.observe_head_seen,
            observe_interval: poll_interval,
            now,
        }) {
            Ok(DoctorPass { report, standing }) => {
                if alert_and_advance(&mut state.last_fingerprint, &report) {
                    for check in report.violations() {
                        tracing::error!(
                            target: "aether_chassis_bloomery::doctor::alert",
                            invariant = check.name,
                            statement = check.statement,
                            divergences = %check.divergences.join("; "),
                            "doctor invariant violated",
                        );
                    }
                }
                // Inherited so `DoctorTick` settlement includes the API store;
                // detached would let `GET /view` overtake apply.
                ctx.actor::<BloomeryApiCapability>().send(&LatestDoctorReport::from(report));

                redispatch_standing(ctx, &mut state.undispatched_seen, &standing, poll_interval, now);
            }
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                %error,
                "doctor pass failed to collect live state",
            ),
        }
    }
}

struct CollectRequest<'a> {
    store: &'a mut dyn StoreBackend,
    source: Option<&'a SourceShell>,
    correspondence: Option<&'a SharedCorrespondence>,
    worktree_base: &'a Path,
    lanes_running: bool,
    started_nonces: &'a [String],
    replica_seen: &'a mut BTreeMap<u64, Instant>,
    replica_passes: &'a mut BTreeMap<u64, u32>,
    surface_seen: &'a mut BTreeMap<(BloomId, WorkpieceId), Instant>,
    unresolved_head_seen: &'a mut Option<(String, Instant)>,
    observe_head_seen: &'a mut Option<(String, Instant)>,
    /// This reactor's poll interval: the observe cadence the lag budget is
    /// measured against (#6025).
    observe_interval: Duration,
    now: Instant,
}

/// One doctor pass: the report, and the members it found standing with no lane
/// and no dispatch — the set [`redispatch_standing`] ages and acts on.
struct DoctorPass {
    report: DoctorReport,
    standing: Vec<StandingMember>,
}

/// One member the estate left standing, with what a fresh dispatch would run:
/// the stage its cursor sits at, the artifact that stage would judge, and the
/// machinery roll a redispatch fault would record.
struct StandingMember {
    bloom: BloomId,
    workpiece: WorkpieceId,
    stage: StageId,
    subject: Digest,
    /// The roll a fault admitted now would be. It distinguishes one redispatch
    /// of this member at this stage from the next: the idempotency key carries
    /// it, so a member lost twice is re-dispatched twice instead of the second
    /// admission being discarded as a replay of the first.
    roll: u32,
}

fn collect_and_evaluate(request: &mut CollectRequest<'_>) -> rusqlite::Result<DoctorPass> {
    let replayed = replay(request.store)?;
    let outstanding_rows = outstanding(request.store)?;
    let outstanding: Vec<OpenDispatch<'_>> = outstanding_rows
        .iter()
        .map(|row| OpenDispatch { nonce: row.nonce.as_str(), workpiece: row.workpiece.as_str() })
        .collect();
    let evidence = evidence_nonces(request.worktree_base);
    let evidence_refs: Vec<&str> = evidence.iter().map(String::as_str).collect();
    let claims = request.source.map(SourceShell::enumerate_claims).transpose().unwrap_or_else(|error| {
        tracing::warn!(
            target: "aether_chassis_bloomery::doctor",
            %error,
            "doctor could not enumerate claim refs",
        );
        None
    });
    let claims = claims.unwrap_or_default();
    let (actual_head, actual_head_sha) = actual_daily_head(request.source, request.correspondence);
    let unresolved_head_age =
        observe_unresolved_head(actual_head, actual_head_sha.as_deref(), request.unresolved_head_seen, request.now);
    let last_observe_age = observe_actual_head(actual_head_sha.as_deref(), request.observe_head_seen, request.now);
    let started_refs: Vec<&str> = request.started_nonces.iter().map(String::as_str).collect();
    let pairs = request.correspondence.as_ref().map_or_else(Vec::new, |store| match store.pairs() {
        Ok(pairs) => pairs,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                %error,
                "doctor could not list correspondence",
            );
            Vec::new()
        }
    });
    let replica_topics = request.store.drain_outbox(Some(Topic::SourceReplica.as_str()))?;
    let replica = observe_replica(&replica_topics, request.replica_seen, request.replica_passes, request.now);
    let surface_parks = observe_surface_parks(&replayed.snapshot, request.surface_seen, request.now);
    let candidate_ref_trees = candidate_ref_trees(request.source, &replayed.snapshot);
    let ancestry = |from: &Digest, to: &Digest| request.source.and_then(|source| source.is_fast_forward(from, to).ok());
    let known_flakes = request
        .store
        .known_flakes(KNOWN_FLAKE_CANDIDATES)?
        .into_iter()
        .map(|row| KnownFlake { test_id: row.test_id, candidates: row.candidates })
        .collect::<Vec<_>>();

    let live = LiveState {
        snapshot: &replayed.snapshot,
        claims: &claims,
        actual_head,
        actual_head_sha: actual_head_sha.as_deref(),
        correspondence: &pairs,
        landed_heads: &replayed.landed_heads,
        land_sequences: &replayed.land_sequences,
        journaled_heads: &replayed.journaled_heads,
        ancestry: Some(&ancestry),
        replica: &replica,
        surface_parks: &surface_parks,
        outstanding: &outstanding,
        started_nonces: &started_refs,
        lanes_running: request.lanes_running,
        evidence_nonces: &evidence_refs,
        unresolved_head_age,
        observe_interval: request.observe_interval,
        last_observe_age,
        candidate_ref_trees: &candidate_ref_trees,
        known_flakes: &known_flakes,
    };

    // Read off the same pass the report is evaluated from, so the members the
    // reactor re-dispatches are exactly the ones the report names.
    let standing = standing_members(&replayed.snapshot, &undispatched_members(&live));
    Ok(DoctorPass { report: evaluate(&live), standing })
}

/// Resolve each standing member against the snapshot into what a redispatch
/// would run. A member the snapshot cannot answer for — no sealed membership,
/// or no cursor — is dropped rather than dispatched blind.
fn standing_members(snapshot: &Snapshot, standing: &[(BloomId, WorkpieceId)]) -> Vec<StandingMember> {
    standing
        .iter()
        .filter_map(|(bloom, workpiece)| {
            let record = snapshot.blooms.get(bloom)?;
            let member = record.spec.members().iter().find(|member| member.workpiece == *workpiece)?;
            let cursor = record.progress.get(workpiece)?;
            let roll = snapshot
                .member_machinery(bloom, workpiece)
                .filter(|fault| fault.stage == cursor.stage)
                .map_or(1, |fault| fault.rolls.saturating_add(1));

            Some(StandingMember {
                bloom: *bloom,
                workpiece: workpiece.clone(),
                stage: cursor.stage,
                subject: cursor.candidate.map_or(member.scope_revision, |held| held.tree),
                roll,
            })
        })
        .collect()
}

/// Read the candidate ref of every member an active bloom could still fold.
///
/// Scoped to members that already have a candidate to compare against, in
/// blooms that have not landed: the read is one ref plus one commit per member,
/// and asking it of a landed bloom's retired namespace would cost the same and
/// mean nothing. A source that cannot answer — unconfigured, an absent ref, a
/// tree with no correspondence — contributes no row, so the invariant reports
/// drift and never absence.
fn candidate_ref_trees(source: Option<&SourceShell>, snapshot: &Snapshot) -> Vec<(BloomId, WorkpieceId, Digest)> {
    let Some(source) = source else {
        return Vec::new();
    };
    let mut trees = Vec::new();
    for (bloom, record) in &snapshot.blooms {
        if !is_active_unlanded(record.status) {
            continue;
        }
        // A set, because a resolved member appears on both sides and the read
        // costs a ref plus a commit each time.
        let mut members: BTreeSet<&WorkpieceId> = BTreeSet::new();
        members.extend(record.claims.keys());
        members
            .extend(record.progress.iter().filter(|(_, cursor)| cursor.candidate.is_some()).map(|(member, _)| member));

        for workpiece in members {
            match source.candidate_ref_tree(bloom, &workpiece.0) {
                Ok(Some(tree)) => trees.push((*bloom, workpiece.clone(), tree)),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    target: "aether_chassis_bloomery::doctor",
                    workpiece = %workpiece.0,
                    %error,
                    "doctor could not read a candidate ref",
                ),
            }
        }
    }
    trees
}

struct Replay {
    snapshot: Snapshot,
    landed_heads: Vec<(BloomId, Digest)>,
    land_sequences: Vec<(BloomId, u64)>,
    journaled_heads: Vec<(Digest, u64)>,
}

fn replay(store: &mut dyn StoreBackend) -> rusqlite::Result<Replay> {
    let mut configs = ResolvedConfigs::default();
    for record in store.load_configs()? {
        let Some(address) = Digest::from_slice(&record.digest) else {
            continue;
        };
        configs.insert(address, record.kind, record.bytes, record.schema_digest);
    }

    let mut snapshot = Snapshot::default();
    let mut landed_heads = Vec::new();
    let mut land_sequences = Vec::new();
    let mut journaled_heads = Vec::new();
    for record in store.replay_journal()? {
        let Ok(event) = decode_recorded_event(&record.event, record.event_schema.as_deref()) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                sequence = record.sequence,
                "doctor: journal event did not decode; skipping",
            );
            continue;
        };
        let Ok(decisions) = decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref())
        else {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                sequence = record.sequence,
                "doctor: journal decisions did not decode; skipping",
            );
            continue;
        };
        snapshot = snapshot.apply(&event, &decisions, &configs);
        match event.fact {
            Fact::Land { bloom, new_head }
                if snapshot.blooms.get(&bloom).is_some_and(|record| record.status == BloomStatus::Landed) =>
            {
                landed_heads.retain(|(id, _)| id != &bloom);
                landed_heads.push((bloom, new_head));
                land_sequences.retain(|(id, _)| id != &bloom);
                land_sequences.push((bloom, record.sequence));
                journaled_heads.push((new_head, record.sequence));
            }
            Fact::ObserveMainline { head } => journaled_heads.push((head, record.sequence)),
            _ => {}
        }
    }
    Ok(Replay { snapshot, landed_heads, land_sequences, journaled_heads })
}

struct OutstandingRow {
    nonce: String,
    workpiece: String,
}

fn outstanding(store: &mut dyn StoreBackend) -> rusqlite::Result<Vec<OutstandingRow>> {
    let mut rows = Vec::new();
    for nonce in store.list_outstanding_nonces()? {
        let Some(order) = store.lookup_order(&nonce)? else {
            continue;
        };
        rows.push(OutstandingRow { nonce: order.nonce, workpiece: order.workpiece });
    }
    Ok(rows)
}

/// Re-dispatch each member that has stood with no live lane and no pending
/// dispatch for a full poll interval.
///
/// The interval is what separates a handoff between two ticks from a dispatch
/// the host actually lost: a member seen standing for the first time this pass
/// is recorded and left alone, and only one still standing an interval later is
/// acted on. A member that moves in the meantime drops out of the set and its
/// sighting with it.
///
/// The redispatch is a `Fact::MemberExecutorFault` against the member's own
/// stage and subject, so it goes through the machinery the reducer already owns
/// (ADR-0195): it journals what the doctor did, redispatches the *same* artifact
/// under a fresh order, and is bounded — at the sealed stage budget the member
/// wedges instead of being re-dispatched forever. The sighting is forgotten on
/// admission, so a member the host loses again re-ages from scratch.
fn redispatch_standing(
    ctx: &mut NativeCtx<'_>,
    seen: &mut BTreeMap<(BloomId, WorkpieceId), Instant>,
    standing: &[StandingMember],
    poll_interval: Duration,
    now: Instant,
) {
    let live: BTreeSet<(BloomId, WorkpieceId)> =
        standing.iter().map(|member| (member.bloom, member.workpiece.clone())).collect();
    seen.retain(|key, _| live.contains(key));

    for member in standing {
        let key = (member.bloom, member.workpiece.clone());
        let first = *seen.entry(key.clone()).or_insert(now);
        if now.saturating_duration_since(first) < poll_interval {
            continue;
        }

        let event = redispatch_fault(member);
        let bytes = match to_vec(&event) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::doctor",
                    %error,
                    "doctor redispatch failed to encode its fault",
                );
                continue;
            }
        };

        ctx.actor::<ControlCore>().send(&Admit { event: bytes });
        tracing::error!(
            target: "aether_chassis_bloomery::doctor::alert",
            bloom = %member.bloom.0.to_hex(),
            workpiece = %member.workpiece.0,
            stage = ?member.stage,
            roll = member.roll,
            "doctor re-dispatched a non-terminal member with no live lane and no pending dispatch",
        );
        seen.remove(&key);
    }
}

/// The fault one redispatch admits. Both the idempotency key and the evidence
/// detail carry the roll, so a second loss of the same member at the same stage
/// is a distinct fact rather than a replay the journal discards.
fn redispatch_fault(member: &StandingMember) -> Event {
    let stamp = format!(
        "doctor redispatch\n{}\n{}\n{:?}\n{}\n",
        member.bloom.0.to_hex(),
        member.workpiece.0,
        member.stage,
        member.roll
    );

    Event {
        idempotency_key: IdempotencyKey(format!(
            "doctor-redispatch:{}:{}:{:?}:{}",
            member.bloom.0.to_hex(),
            member.workpiece.0,
            member.stage,
            member.roll
        )),
        fact: Fact::MemberExecutorFault {
            bloom: member.bloom,
            workpiece: member.workpiece.clone(),
            stage: member.stage,
            evidence: Evidence {
                subject: member.subject,
                kind: EvidenceKind::ExecutorFault,
                detail: Digest::of_wire_bytes(stamp.as_bytes()),
            },
        },
    }
}

fn evidence_nonces(worktree_base: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(worktree_base) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            entry.file_name().to_str().and_then(|name| name.strip_suffix("-evidence")).map(str::to_owned)
        })
        .collect()
}

fn actual_daily_head(
    source: Option<&SourceShell>,
    correspondence: Option<&SharedCorrespondence>,
) -> (Option<Digest>, Option<String>) {
    let Some(source) = source else {
        return (None, None);
    };
    let sha = match source.mainline_head_sha() {
        Ok(sha) => sha,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                %error,
                "doctor could not read the daily ref head",
            );
            return (None, None);
        }
    };
    let digest = correspondence.and_then(|store| {
        let object = GitObjectId::from_hex(&sha).map(BackendObjectId::from)?;
        match store.resolve_digest(&object) {
            Ok(digest) => digest,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::doctor",
                    %error,
                    "doctor could not resolve the daily ref through correspondence",
                );
                None
            }
        }
    });
    (digest, Some(sha))
}

/// Age a daily sha this process has seen without a correspondence, the way
/// [`observe_replica`] ages an undelivered topic: a sha seen for the first
/// time starts now, one seen before keeps its first sighting, and a resolved
/// head (or a different sha) drops the sighting so a later miss starts fresh.
fn observe_unresolved_head(
    actual_head: Option<Digest>,
    actual_head_sha: Option<&str>,
    seen: &mut Option<(String, Instant)>,
    now: Instant,
) -> Option<Duration> {
    let Some(sha) = actual_head_sha.filter(|_| actual_head.is_none()) else {
        *seen = None;
        return None;
    };
    Some(sighted_age(sha, seen, now))
}

/// Age the live daily sha this process has seen, resolved or not (#6025).
/// The head checks read it as the lag budget: a mismatch against a young sha
/// is a ref that moved, against an old one an observer that stopped. A missed
/// read keeps the previous sighting rather than granting a fresh budget, and
/// a different sha restarts it — the same first-sighting rule as
/// [`observe_unresolved_head`], without its clearing, because an unreadable
/// ref is not evidence the observer caught up.
fn observe_actual_head(
    actual_head_sha: Option<&str>,
    seen: &mut Option<(String, Instant)>,
    now: Instant,
) -> Option<Duration> {
    actual_head_sha.map(|sha| sighted_age(sha, seen, now))
}

/// First-sighting age of one live sha: seen for the first time starts now,
/// seen before keeps its first sighting.
fn sighted_age(sha: &str, seen: &mut Option<(String, Instant)>, now: Instant) -> Duration {
    let first = match seen {
        Some((prev, first)) if prev == sha => *first,
        _ => now,
    };
    *seen = Some((sha.to_owned(), first));
    now.saturating_duration_since(first)
}

fn observe_replica(
    entries: &[OutboxEntry],
    seen: &mut BTreeMap<u64, Instant>,
    passes: &mut BTreeMap<u64, u32>,
    now: Instant,
) -> Vec<ReplicaObservation> {
    let live: BTreeMap<u64, Instant> =
        entries.iter().map(|entry| (entry.sequence, *seen.get(&entry.sequence).unwrap_or(&now))).collect();
    seen.retain(|sequence, _| live.contains_key(sequence));
    passes.retain(|sequence, _| live.contains_key(sequence));
    for (sequence, first) in &live {
        seen.insert(*sequence, *first);
        *passes.entry(*sequence).or_insert(0) += 1;
    }
    live.into_iter()
        .map(|(sequence, first)| ReplicaObservation {
            sequence,
            age: now.saturating_duration_since(first),
            consecutive_failures: passes.get(&sequence).copied().unwrap_or(1),
        })
        .collect()
}

/// Age every member awaiting a surface amendment against this process's own
/// clock, the way [`observe_replica`] ages an undelivered topic: a member seen
/// for the first time starts now, one seen before keeps its first sighting,
/// and one whose park has cleared is dropped so a later park starts fresh.
///
/// Only a member that can still be answered is observed. A landed or
/// superseded bloom's parked entry is history, and a withdrawn member's
/// request has nobody left to answer it (#5327) — the reducer clears the entry
/// when the member *moves*, not when it leaves, so reporting either would
/// leave a row nobody can ever clear.
fn observe_surface_parks(
    snapshot: &Snapshot,
    seen: &mut BTreeMap<(BloomId, WorkpieceId), Instant>,
    now: Instant,
) -> Vec<SurfaceParkObservation> {
    let answerable = snapshot.surface_requests.iter().filter_map(|(bloom, members)| {
        let record = snapshot.blooms.get(bloom)?;
        is_active_unlanded(record.status).then(move || {
            members
                .keys()
                .filter(move |workpiece| !record.withdrawn.contains_key(*workpiece))
                .map(|workpiece| (*bloom, workpiece.clone()))
        })
    });

    let live: BTreeMap<(BloomId, WorkpieceId), Instant> = answerable
        .flatten()
        .map(|key| {
            let first = seen.get(&key).copied().unwrap_or(now);
            (key, first)
        })
        .collect();
    seen.retain(|key, _| live.contains_key(key));
    for (key, first) in &live {
        seen.insert(key.clone(), *first);
    }

    live.into_iter()
        .map(|((bloom, workpiece), first)| SurfaceParkObservation {
            bloom,
            workpiece,
            age: now.saturating_duration_since(first),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{CheckResult, DoctorReport};
    use super::alert_and_advance;

    fn dirty(name: &str, divergence: &str) -> DoctorReport {
        DoctorReport {
            checks: vec![CheckResult {
                name: name.into(),
                statement: "the property held".into(),
                passed: false,
                pending: false,
                divergences: vec![divergence.into()],
            }],
        }
    }

    fn waiting(name: &str, divergence: &str) -> DoctorReport {
        DoctorReport {
            checks: vec![CheckResult {
                name: name.into(),
                statement: "the property held".into(),
                passed: false,
                pending: true,
                divergences: vec![divergence.into()],
            }],
        }
    }

    fn clean(name: &str) -> DoctorReport {
        DoctorReport {
            checks: vec![CheckResult {
                name: name.into(),
                statement: "the property held".into(),
                passed: true,
                pending: false,
                divergences: Vec::new(),
            }],
        }
    }

    #[test]
    fn alert_fingerprint_is_idempotent_on_the_same_failing_set() {
        // The plausible bug: a stable violation re-alerts every poll, or a
        // newly-loud set is silent because the fingerprint did not advance.
        let mut last = String::new();
        let first = dirty("observed_head_equals_daily_head", "observed aa != actual bb");
        assert!(alert_and_advance(&mut last, &first), "the first dirty pass is newly loud");
        assert!(!alert_and_advance(&mut last, &first), "the same failing set is not re-posted");
        let louder = dirty("claim_refs_name_active_blooms", "refs/bloomery/claims/issue-5175");
        assert!(alert_and_advance(&mut last, &louder), "a different failing set is newly loud");
        assert!(
            !alert_and_advance(&mut last, &clean("observed_head_equals_daily_head")),
            "a clean pass is not an alert"
        );
        assert!(alert_and_advance(&mut last, &first), "a dirty set after a clean pass is newly loud again");
    }

    #[test]
    fn a_pending_check_never_alerts_and_never_dirties_the_fingerprint() {
        // #6025: the wait is not a violation, so the alert path must not see
        // it — neither as a post, nor as a fingerprint change that would
        // swallow or invent the next real alert.
        let mut last = String::new();
        let waiting = waiting("observed_head_equals_daily_head", "observed aa != actual bb");
        assert!(!alert_and_advance(&mut last, &waiting), "a waiting pass is not an alert");
        assert!(last.is_empty(), "a waiting pass leaves no fingerprint: {last:?}");
        assert!(!alert_and_advance(&mut last, &waiting), "a stable wait stays silent");
        let violated = dirty("observed_head_equals_daily_head", "observed aa != actual bb");
        assert!(alert_and_advance(&mut last, &violated), "the same lag past its budget is newly loud");
    }
}
