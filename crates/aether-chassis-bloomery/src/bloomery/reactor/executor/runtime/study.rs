//! The bloom-level reader drain (ADR-0216): the `retrospect.read` lane a
//! landing decided, submitted over what the bloom landed.
//!
//! Modelled on `drain_and_dispatch_aggregate` — same ack-prefix, park, and
//! backoff semantics, the same bloom-level order record with no member axis,
//! and the same `ModelOverride` overlay, because the reader is a model lane and
//! the seat it runs on has to be the one the bloom sealed. Two steps of that
//! sibling are absent, each for a property of the read rather than an
//! optimisation:
//!
//! - **No `bloom_still_live`.** That check reads `active_membership`, and the
//!   landing this order was decided in released every membership the bloom
//!   held. A reader gated on it would be retired undispatched on every tick,
//!   forever — the check means "this bloom is still walking", and the whole
//!   point of this lane is that it runs after the walk is over.
//! - **No task composition.** The critic is handed the sealed work orders it
//!   judges the fold against; the reader is handed the landed range and asked
//!   what it sees. Its process instructions come from the ADR-0214 bundle the
//!   provenance gate resolves, and the subject rides `inputs[0]` — nothing here
//!   authors prose for it.
//!
//! A refused or faulted submit is not escalated anywhere. The bloom has landed;
//! the only thing at stake is whether its study exists.
//!
//! Whether the read is dispatched at all is a host knob (ADR-0216 §4). The seat
//! is the owner's call and the ADR does not make it, so it cannot ride on the
//! instruction bundle: the ADR-0214 gate demands every instruction field be
//! filled before any model lane dispatches, so the first authorized bundle
//! carries the reader's text whether or not the owner wants a standing read.
//! The knob is what separates "these instructions exist" from "spend an opus
//! seat on every landing", and a declined read is journaled rather than
//! skipped.

use aether_bloomery::{BloomId, ConfigScopes, ModelOverride, StageId, StudyPayload, Topic, WorkHandle, WorkpieceId};
use aether_bloomery_github::short_hex;
use aether_data::wire::from_bytes;

use crate::artifacts::ArtifactsCapabilityState;
use crate::bloomery::dispatch_model;
use crate::bloomery::executor::{ExecutorPort, Settled};
use crate::bloomery::intake::{DispatchRecord, dispatch_and_record, dispatch_nonce};
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::provenance::{ProvenanceRefusal, journal_refusal};
use crate::store::{StoreBackend, StoreConfigError, resolve_config};

use super::transformation_has_subject;

/// Drain the study topic and submit each entry through the executor under a
/// bloom-level order record.
///
/// `reader_enabled` is the host's answer to the seat ADR-0216 §4 leaves open.
/// Off, each drained entry is journaled as a refusal instead of submitted, so
/// the read is declined once, legibly, and the row is acked rather than
/// re-driven every tick.
///
/// Returns the newly-tracked handles, the highest contiguously-submitted outbox
/// sequence to ack, and the sequence of a transient submit failure that stopped
/// the drain — the same triple every other drain returns.
pub(super) fn drain_and_dispatch_study(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    reader_enabled: bool,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Study)?;
    let mut handles = Vec::new();
    let mut ack_through = None;
    let mut transient_failure = None;
    for entry in entries {
        let Ok(mut payload) = from_bytes::<StudyPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "study outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        if !transformation_has_subject(&payload.transformation.inputs, entry.sequence, "study") {
            break;
        }

        // ADR-0216 §4 leaves the reader's seat to the owner, so whether this
        // host spends one is boot configuration. Answered here rather than
        // inside the gate, and before the sealed override is resolved: a read
        // this host will not dispatch has no seat to look up, and a resolve
        // that failed would park an entry over a lane that was never going to
        // run. The refusal takes the same road every provenance refusal takes,
        // so a landed bloom carries a study that says it is missing and why.
        if !reader_enabled {
            let record = study_record(entry.sequence, payload);
            tracing::info!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                bloom = %short_hex(&record.bloom.0),
                "the bloom-level reader is not enabled on this host; journaling the study as missing",
            );
            journal_refusal(store, &record, &ProvenanceRefusal::ReaderDisabled);
            ack_through = Some(entry.sequence);
            continue;
        }

        // The reader runs on the bloom's own sealed seat (ADR-0174), resolved
        // host-side so the receipt attests the agent that actually ran. A sealed
        // address that will not resolve parks the read rather than falling
        // through to the catalog default — the same divergence the critic's
        // drain refuses, and the reader has one attempt to spend.
        let model_override = match resolve_config::<ModelOverride>(store, ConfigScopes::bloom_wide(&payload.configs)) {
            Ok(override_) => override_.unwrap_or_default(),
            Err(StoreConfigError::Store(error)) => return Err(error),
            Err(error) => {
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = %short_hex(&payload.bloom),
                    %error,
                    "sealed configuration did not resolve; parking the study rather than reading on the default",
                );
                ack_through = Some(entry.sequence);
                continue;
            }
        };
        payload.transformation.model = Some(dispatch_model(StageId::Study, &payload.profile, &model_override));

        let record = study_record(entry.sequence, payload);
        match dispatch_and_record(executor, store, artifacts, &record, now_unix_millis) {
            Ok(Settled::Answered(handle)) => {
                handles.push(handle);
                ack_through = Some(entry.sequence);
            }
            Ok(Settled::InFlight) => break,
            Err(error) if error.is_permanent() => {
                // Including a provenance refusal (ADR-0214): the reader is a
                // model lane, so it passes the same gate, and a bloom whose
                // sealed bundle is not authorized simply does not get read.
                // `dispatch_and_record` journals that refusal itself.
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = %short_hex(&record.bloom.0),
                    nonce = %record.nonce.0,
                    %error,
                    "study submit refused permanently; parking the entry instead of re-driving",
                );
                ack_through = Some(entry.sequence);
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    %error,
                    "study submit/record failed; stopping the ack prefix to re-drive",
                );
                transient_failure = Some(entry.sequence);
                break;
            }
        }
    }
    Ok((handles, ack_through, transient_failure))
}

/// The bloom-level order record one drained study entry becomes.
///
/// Built for both roads out of the drain — the submit and the declined read —
/// because a refusal is journaled against the order it *would* have been, and a
/// record assembled differently on the refusing side would file the fault under
/// a nonce and a subject the dispatching side never used.
fn study_record(sequence: u64, payload: StudyPayload) -> DispatchRecord {
    // The evidence-binding subject is the landing receipt's digest the reducer
    // pinned as `inputs[0]` — also the displayed digest the returning verdict
    // must bind. Present because the caller checked it before calling here.
    let displayed = payload.transformation.inputs[0];

    DispatchRecord {
        nonce: dispatch_nonce(sequence),
        bloom: BloomId(payload.bloom),
        // A bloom-level order has no member axis (ADR-0153): the stage
        // discriminates at intake, and the empty workpiece never routes.
        workpiece: WorkpieceId(String::new()),
        profile: payload.profile,
        scope_revision: displayed,
        candidate: displayed,
        displayed_digest: displayed,
        stage: StageId::Study,
        transformation: payload.transformation,
        // The bloom-wide registry (ADR-0174): the reader has no member axis, so
        // this is the only scope the overlay and the ADR-0214 provenance gate
        // walk.
        configs: payload.configs,
        instruction_bundle: None,
        prompt_manifest: None,
    }
}
