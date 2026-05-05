//! `hub.claude.broadcast` cap. Issue 576: broadcast as a real
//! catch-all chassis cap, replacing the closure-sink path the
//! substrate held in `SubstrateBoot::build`. Catch-all means every
//! kind sent to this cap fans out to every attached MCP session
//! through [`HubOutbound::egress_broadcast`] — there is no per-kind
//! handler, just a `#[fallback]` that lifts each envelope into the
//! outbound bridge.
//!
//! The macro auto-emits a blanket `impl<K: Kind> HandlesKind<K> for
//! BroadcastCapability {}` (issue 576's relaxation) so typed sends like
//! `ctx.actor::<BroadcastCapability>().send(&payload)` compile against any
//! payload the caller wants to broadcast — pairing the runtime
//! catch-all with type-system catch-all so the strict-receiver shape
//! of every other cap stays honest by contrast.
//!
//! ADR-0008 (observation path) describes the user-facing semantics.
//! The cap is universal across desktop / headless / hub /
//! test-bench; chassis bins chain `with_actor::<BroadcastCapability>(())`
//! alongside the rest of the cap set.
//!
//! [`HubOutbound::egress_broadcast`]: aether_substrate::outbound::HubOutbound::egress_broadcast

// `HUB_BROADCAST_MAILBOX_NAME` must be importable at file root because
// `#[bridge]` lifts the `const NAMESPACE` expression onto an always-on
// `impl Actor for X` block emitted as a sibling of the mod (outside
// any cfg gate). Aether-kinds is wasm-compatible so the import doesn't
// need cfg gating.
use aether_kinds::mailboxes::HUB_BROADCAST_MAILBOX_NAME;

#[aether_actor::bridge]
mod native {
    use std::sync::Arc;

    use aether_actor::{Actor, actor};
    use aether_substrate::capability::{BootError, Envelope};
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::outbound::HubOutbound;

    /// `hub.claude.broadcast` mailbox cap. Holds an `Arc<HubOutbound>`
    /// grabbed at boot from `NativeInitCtx::mailer().outbound()`.
    /// Every kind addressed at this mailbox runs through the
    /// `#[fallback]` and lifts into [`HubOutbound::egress_broadcast`]
    /// so MCP sessions attached to the hub see the fan-out.
    pub struct BroadcastCapability {
        outbound: Arc<HubOutbound>,
    }

    #[actor]
    impl NativeActor for BroadcastCapability {
        type Config = ();
        const NAMESPACE: &'static str = HUB_BROADCAST_MAILBOX_NAME;

        fn init(_: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            // The substrate boot wires `HubOutbound` into the mailer
            // before chassis caps run their `init`. Caps that boot on
            // a substrate without an outbound (single-host dev, the
            // hub chassis itself) get `None` here and refuse the boot
            // loud — broadcast has no fallback path of its own.
            let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
                BootError::Other("BroadcastCapability requires a wired HubOutbound".into())
            })?;
            Ok(Self { outbound })
        }

        /// Catch-all handler — runs for every envelope addressed at
        /// `hub.claude.broadcast` regardless of kind. Lifts the
        /// envelope into [`HubOutbound::egress_broadcast`] so attached
        /// MCP sessions see the fan-out.
        ///
        /// # Agent
        /// Components push observation mail at this mailbox to surface
        /// it to every attached Claude session. Fire-and-forget; no
        /// reply. Use any kind the engine has registered — the
        /// fallout reaches the session as `receive_mail` items
        /// decoded against the engine's kind descriptor (ADR-0020).
        #[fallback]
        fn on_any(&self, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
            if env.kind_name.is_empty() {
                tracing::warn!(
                    target: "aether_substrate::broadcast",
                    "{} received mail with unregistered kind — dropping",
                    BroadcastCapability::NAMESPACE,
                );
                return;
            }
            // ADR-0042: preserve the auto-minted correlation
            // end-to-end so MCP-side tooling can correlate broadcasts
            // with their originating sends if it wants to. Most
            // broadcast uses are fire-and-forget and ignore it.
            self.outbound.egress_broadcast(
                &env.kind_name,
                env.payload.clone(),
                env.origin.clone(),
                env.sender.correlation_id,
            );
        }
    }
}
