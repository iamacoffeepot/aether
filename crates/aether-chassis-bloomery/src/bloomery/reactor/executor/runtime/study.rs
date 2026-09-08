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

use aether_bloomery::{BloomId, ConfigScopes, ModelOverride, StageId, StudyPayload, Topic, WorkHandle, WorkpieceId};
use aether_bloomery_github::short_hex;
use aether_data::wire::from_bytes;

use crate::bloomery::dispatch_model;
use crate::bloomery::executor::ExecutorPort;
use crate::bloomery::intake::{DispatchRecord, dispatch_and_record, dispatch_nonce};
use crate::bloomery::outbox::TopicOutbox;
use crate::store::{StoreBackend, StoreConfigError, resolve_config};

use super::transformation_has_subject;

/// Drain the study topic and submit each entry through the executor under a
/// bloom-level order record.
///
/// Returns the newly-tracked handles, the highest contiguously-submitted outbox
/// sequence to ack, and the sequence of a transient submit failure that stopped
/// the drain — the same triple every other drain returns.
pub(super) fn drain_and_dispatch_study(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Study)?;
    let mut handles = Vec::new();
    let mut ack_through = None;
    let mut transient_failure = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<StudyPayload>(&entry.payload) else {
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
        let mut transformation = payload.transformation;
        transformation.model = Some(dispatch_model(StageId::Study, &payload.profile, &model_override));
        // The evidence-binding subject is the landing receipt's digest the
        // reducer pinned as `inputs[0]` — also the displayed digest the
        // returning verdict must bind.
        let displayed = transformation.inputs[0];
        let record = DispatchRecord {
            nonce: dispatch_nonce(entry.sequence),
            bloom: BloomId(payload.bloom),
            // A bloom-level order has no member axis (ADR-0153): the stage
            // discriminates at intake, and the empty workpiece never routes.
            workpiece: WorkpieceId(String::new()),
            profile: payload.profile,
            scope_revision: displayed,
            candidate: displayed,
            displayed_digest: displayed,
            stage: StageId::Study,
            transformation,
            // The bloom-wide registry (ADR-0174): the reader has no member axis,
            // so this is the only scope the overlay and the ADR-0214 provenance
            // gate walk.
            configs: payload.configs,
        };
        match dispatch_and_record(executor, store, &record, now_unix_millis) {
            Ok(handle) => {
                handles.push(handle);
                ack_through = Some(entry.sequence);
            }
            Err(error) if error.is_permanent() => {
                // Including a provenance refusal (ADR-0214): the reader is a
                // model lane, so it passes the same gate, and a bloom whose
                // sealed bundle is not authorized simply does not get read.
                // `dispatch_and_record` journals that refusal itself.
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = %short_hex(&payload.bloom),
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
