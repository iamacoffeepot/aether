//! `hub.claude.broadcast` synthetic actor marker (issue 552 stage
//! 2e). Pre-stage-2e the broadcast mailbox was registered as a raw
//! closure sink in `aether_substrate::SubstrateBoot::build` — there
//! was no struct, no [`Actor`] impl, no namespace const. Senders
//! addressed it by hard-coded name (`"hub.claude.broadcast"`).
//!
//! Stage 2e introduces [`HubBroadcast`] as a marker-only actor so
//! components can typed-send to broadcast through the same shape
//! every other actor uses. The actual fan-out to attached MCP
//! sessions still lives on the closure registered at boot — there
//! is no [`NativeActor`] impl for [`HubBroadcast`] because the
//! broadcast machinery doesn't fit the per-mail
//! `Arc<Self>`-dispatcher shape (it lifts each envelope into a
//! `HubOutbound::egress_broadcast` call instead). Once Stage 3
//! migrates senders onto `ctx.send_to::<R>` the [`HandlesKind<K>`]
//! impls go on this struct as well; today the type is exported only
//! so consumers can reach for it ahead of that work.
//!
//! [`Actor`]: aether_actor::Actor
//! [`NativeActor`]: aether_substrate::native_actor::NativeActor
//! [`HandlesKind<K>`]: aether_actor::HandlesKind

use aether_actor::{Actor, Singleton};

/// Broadcast fan-out to every attached MCP session (ADR-0008
/// observation path). Marker-only — there is no [`NativeActor`]
/// impl; the substrate registers a closure sink under
/// `Self::NAMESPACE` at boot that lifts envelopes into
/// `HubOutbound::egress_broadcast`.
///
/// [`NativeActor`]: aether_substrate::native_actor::NativeActor
pub struct HubBroadcast;

impl Actor for HubBroadcast {
    const NAMESPACE: &'static str = "hub.claude.broadcast";
}

impl Singleton for HubBroadcast {}
