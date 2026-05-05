//! Synthetic actor marker for the broadcast mailbox (issue 552
//! Stage 3). The mailbox itself is a closure sink registered in
//! [`SubstrateBoot::build`] — there is no [`NativeActor`] impl
//! because broadcast lifts each envelope into
//! [`HubOutbound::egress_broadcast`] rather than dispatching through
//! a per-handler `Arc<Self>` machinery.
//!
//! Pre-Stage-3 the substrate's broadcast routing read `&str`-named
//! lookups (`HUB_CLAUDE_BROADCAST` const + `MailboxId::from_name`).
//! [`HubBroadcast::MAILBOX_ID`] is a `const`-evaluated mailbox id so
//! every chassis frame loop / scheduler death path / outbound bridge
//! can reach for one symbol instead of duplicating the
//! `mailbox_id_from_name` call site.
//!
//! Pre-Stage-3 a stub copy lived in `aether-capabilities`; that home
//! was wrong because the broadcast machinery itself lives in this
//! crate (the closure sink + outbound bridge), and `aether-capabilities`
//! is the chassis-cap layer above it. ADR-0008 (observation path) for
//! the user-facing semantics.
//!
//! [`SubstrateBoot::build`]: crate::SubstrateBoot::build
//! [`HubOutbound::egress_broadcast`]: crate::HubOutbound::egress_broadcast
//! [`NativeActor`]: crate::native_actor::NativeActor

use aether_actor::{Actor, Singleton};
use aether_data::{MailboxId, mailbox_id_from_name};

/// Broadcast fan-out to every attached MCP session (ADR-0008
/// observation path). Marker-only: no [`NativeActor`] impl, no
/// fields. The substrate registers a closure sink under
/// [`Self::NAMESPACE`] at boot that lifts envelopes into
/// `HubOutbound::egress_broadcast`.
///
/// [`NativeActor`]: crate::native_actor::NativeActor
pub struct HubBroadcast;

impl Actor for HubBroadcast {
    const NAMESPACE: &'static str = "hub.claude.broadcast";
}

impl Singleton for HubBroadcast {}

impl HubBroadcast {
    /// Const-evaluated mailbox id matching [`Self::NAMESPACE`]. Same
    /// hash any `mailbox_id_from_name` lookup at this name lands at,
    /// just folded into a single symbol so chassis code doesn't
    /// re-do the lookup or thread the resolved id around.
    pub const MAILBOX_ID: MailboxId = mailbox_id_from_name(Self::NAMESPACE);
}
