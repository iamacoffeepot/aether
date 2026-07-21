//! ADR-0155 Claim stage, run in isolation. The chassis lifecycle is
//! five stages (Compose → Claim → Init → Wire → Start); `build` /
//! `build_passive` fuse Claim through Start into one call. This module
//! exposes Claim alone — every composed passive plus the driver type's
//! value-free claim hook reserve their namespaces in the registry, and
//! nothing past Claim runs. It backs
//! [`Builder::claim_namespaces`](super::Builder::claim_namespaces), the
//! seam `--describe` reads to derive its capability roster from the same
//! claim code a real boot runs.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::passive_boot::PassiveBoot;
use crate::chassis::ctx::{ChassisCtx, FallbackRouter, MailboxClaim};
use crate::chassis::error::BootError;
use crate::config::RingCapacities;
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::WakeSink;

/// Run ONLY the Claim pass (phase 1 of the issue-697 multi-pass) over
/// `passives` plus `driver_claim`, then return the set of namespaces
/// registered on `registry`. The three registration contributors all
/// land here: the `with_actor` chain (each passive's `claim`), the
/// driver-as-actor claim hook, and any inline sinks a chassis registered
/// directly on the shared registry before composing the builder.
///
/// No worker pool, no dispatcher threads, no `init` — the claim path
/// only reserves registry slots and builds in-memory per-cap transports,
/// so nothing touches an OS resource. Teardown is the plain drop of the
/// composed passives when this returns; there is no `cleanup_after_failure`
/// walk, because a claim-only application never acquires a runtime
/// resource to release. A claim failure propagates immediately and the
/// partially-populated registry drops with the returned error.
pub(super) fn claim_only(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    aborter: &Arc<dyn FatalAborter>,
    ring_caps: RingCapacities,
    mut passives: Vec<Box<dyn PassiveBoot>>,
    driver_claim: impl FnOnce(&mut ChassisCtx<'_>) -> Result<(), BootError>,
) -> Result<BTreeSet<String>, BootError> {
    // The claim path reaches the `Spawner` through `ChassisCtx::spawner_arc`
    // (namespace ownership claim + per-actor ring caps + per-cap transport
    // construction). A detached wake sink lets us build that `Spawner`
    // without `Pool::start` spawning any worker thread — the Claim stage
    // never schedules a dispatcher slot, so the sink is never drained.
    let actor_registry = Arc::new(crate::ActorRegistry::new());
    let spawner = Arc::new(crate::Spawner::new(
        Arc::clone(registry),
        actor_registry,
        Arc::clone(mailer),
        Arc::clone(aborter),
        WakeSink::detached(),
        ring_caps,
    ));

    let mut fallback: Option<FallbackRouter> = None;
    let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
    // ADR-0155 §4: the driver's Claim hook reserves its inbox here (via
    // `claim_driver_mailbox`), stashing the live `MailboxClaim`. Describe
    // wants only the claimed *namespaces* off the registry, so this stash is
    // read for nothing and drops when `claim_only` returns — the inbox
    // receiver, actor slots, and wake slot the reservation produced never
    // reach a Start stage on this path.
    let mut reserved_driver_mailboxes: Vec<(String, MailboxClaim)> = Vec::new();

    for boot in &mut passives {
        let mut ctx = ChassisCtx::new(
            registry,
            mailer,
            &mut fallback,
            aborter,
            &mut claimed_actor_mailboxes,
            &spawner,
            &mut reserved_driver_mailboxes,
        );
        boot.claim(&mut ctx)?;
    }

    let mut ctx = ChassisCtx::new(
        registry,
        mailer,
        &mut fallback,
        aborter,
        &mut claimed_actor_mailboxes,
        &spawner,
        &mut reserved_driver_mailboxes,
    );
    driver_claim(&mut ctx)?;

    Ok(claimed_namespaces(registry))
}

/// Every namespace registered on `registry`, excluding the synthetic
/// `aether.chassis` router sentinel that `list_mailbox_descriptors`
/// appends — the sentinel is chassis routing infrastructure, not a
/// claimed capability.
fn claimed_namespaces(registry: &Registry) -> BTreeSet<String> {
    registry
        .list_mailbox_descriptors()
        .into_iter()
        .filter(|descriptor| descriptor.id != MailboxId::CHASSIS_MAILBOX_ID)
        .map(|descriptor| descriptor.name)
        .collect()
}
