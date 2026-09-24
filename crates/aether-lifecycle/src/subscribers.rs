//! Publisher surface + fan-out for the lifecycle cap. Holds the
//! `Publishes` / `Publisher` impls the flat
//! `ctx.subscribe::<LifecycleCapability, K>()` verb resolves through
//! (always-on, both transports) and the native [`broadcast_to_subscribers`]
//! fan-out the receive side calls once per advance, which pushes each stage
//! payload to the proven references the cap's subscriber table holds
//! (ADR-0230).

use aether_actor::{Publisher, Publishes};
use aether_data::Kind;
use aether_kinds::{
    InitCaps, InitComponents, LifecycleSubscribeSelf, LifecycleUnsubscribeSelf, Present, Render, Shutdown, Tick,
};

use super::LifecycleCapability;

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_actor::ErasedActorRef;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_actor::ReplyMode;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_data::KindId;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeCtx;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use std::collections::{BTreeMap, BTreeSet};

// The stage kinds this cap broadcasts to its subscriber set, one
// `Publishes` impl each — the compile-time gate on the flat
// `ctx.subscribe::<LifecycleCapability, K>()` verb. The list is the
// ADR-0082 stage vocabulary a chassis lifecycle graph can declare as a
// state; the
// runtime still fail-fasts on a stage *this* chassis's graph omits
// (ADR-0082 §7), so the marker states what the cap can ever emit and
// the reply states what it does emit here.
//
// `Quit` and `LifecycleAdvance` are absent on purpose: they travel
// *into* the cap as signals, never out of it as a broadcast.
impl Publishes<Tick> for LifecycleCapability {}
impl Publishes<InitCaps> for LifecycleCapability {}
impl Publishes<InitComponents> for LifecycleCapability {}
impl Publishes<Render> for LifecycleCapability {}
impl Publishes<Present> for LifecycleCapability {}
impl Publishes<Shutdown> for LifecycleCapability {}

/// The flat subscribe verbs send these self-addressed stage requests; the cap
/// resolves the subscriber from the inbound's host-stamped `Source`
/// (ADR-0083).
impl Publisher for LifecycleCapability {
    type Subscribe = LifecycleSubscribeSelf;
    type Unsubscribe = LifecycleUnsubscribeSelf;

    fn subscribe_request<K: Kind>() -> LifecycleSubscribeSelf
    where
        Self: Publishes<K>,
    {
        LifecycleSubscribeSelf { stage: K::ID.0 }
    }

    fn unsubscribe_request<K: Kind>() -> LifecycleUnsubscribeSelf
    where
        Self: Publishes<K>,
    {
        LifecycleUnsubscribeSelf { stage: K::ID.0 }
    }
}

/// Push the current stage payload to each subscriber as an untyped envelope.
/// Untyped because the broadcast kind is chosen at runtime (the current
/// state's), not a compile-site `K`; the path preserves the inbound
/// `(parent, root)` lineage so settlement counts each child against the root
/// (ADR-0080 §6).
///
/// Takes the proven-target `send_envelope_tracked_to` form (ADR-0230): every
/// row of the table was proven when its subscription was accepted, so the
/// loop hands the send a reference and never unwraps one back to a position.
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub fn broadcast_to_subscribers<A, M: ReplyMode>(
    ctx: &mut NativeCtx<'_, A, M>,
    subscribers: &BTreeMap<KindId, BTreeSet<ErasedActorRef>>,
    stage: KindId,
    payload: &[u8],
) {
    let Some(set) = subscribers.get(&stage) else {
        return;
    };
    for subscriber in set {
        let _ = ctx.send_envelope_tracked_to(*subscriber, stage, payload);
    }
}
