//! The `aether.trace` runtime half (ADR-0122 identity/runtime split):
//! the `aether_substrate`-typed imports and the state struct + its
//! `with_registry` ctor, gated once by this module rather than per-import.
//! The `#[actor] impl` reaches them through the single `use runtime::*`
//! glob in the parent.

use super::{DispatchTraced, TraceDispatchCapability};
use aether_actor::runtime;

#[cfg(not(target_family = "wasm"))]
use super::DispatchTracedAck;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::helpers::resolve_bundle;
pub use aether_substrate::mail::registry::Registry;
pub use std::sync::Arc;

/// `aether.trace` runtime state (ADR-0086 Phase 3c). Holds the
/// substrate registry handle for `DispatchTraced`'s per-envelope name
/// resolution (recipient mailbox name → id, kind name → id). The
/// addressing identity is the distinct ZST `TraceDispatchCapability`.
pub struct TraceDispatchCapabilityState {
    /// Substrate registry handle for `DispatchTraced`'s per-envelope
    /// name resolution. Cloned from `ctx.mailer().registry()` at init;
    /// matches the `RenderCapability` pattern that resolves
    /// `CaptureFrame` mail bundles through the same registry.
    pub registry: Arc<Registry>,
}

impl TraceDispatchCapabilityState {
    /// The single struct-construction site.
    pub fn with_registry(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[runtime]
impl NativeActor for TraceDispatchCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// substrate registry handle.
    type State = TraceDispatchCapabilityState;

    type Config = ();
    // `aether.trace` (matches
    // `aether_kinds::trace::TRACE_MAILBOX_NAME`). Has to be a literal
    // here for the `#[actor]` macro's expansion.
    const NAMESPACE: &'static str = "aether.trace";

    fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<TraceDispatchCapabilityState, BootError> {
        let registry = Arc::clone(ctx.mailer().registry());
        Ok(TraceDispatchCapabilityState::with_registry(registry))
    }

    /// # Agent
    /// Atomic batched dispatch with shared trace root, backing the
    /// MCP `send_mail_traced` tool. Captures this handler's inbound
    /// `MailId` as the batch root, dispatches every spec inheriting
    /// the chain (so all children appear under one tree), and
    /// replies synchronously with [`DispatchTracedAck`] carrying the
    /// root. The caller waits for the wire `ReplyEnd` (chain
    /// settled), then reconstructs the populated tree by walking the
    /// per-actor trace rings from this root (`aether.trace.tail`,
    /// stitched client-side — ADR-0086 Phase 3b). Issue 749.
    ///
    /// **Reply forwarding (issue 1265).** Each child is dispatched
    /// with `reply_to = ctx.reply_target()` (the caller of this
    /// `DispatchTraced` — typically the RPC server holding the wire
    /// `cid`'s in-flight entry) rather than the default
    /// `Component(self_mailbox)`. This trace cap is a re-dispatcher
    /// with no handler for child reply kinds, so without the
    /// forward each child's deferred reply (the ADR-0093
    /// hold-until-resolve dispatch in content-gen caps) lands here and
    /// silently drops, leaving the wire call with no `ReplyEvent`s.
    /// Forwarding lets every child's reply (sync or deferred)
    /// bubble straight to the original caller with the same
    /// `correlation_id`, so the RPC server's `on_any` fallback wraps
    /// each into a `ReplyEvent` on the wire.
    #[handler::single]
    fn on_dispatch_traced(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        batch: DispatchTraced,
    ) -> DispatchTracedAck {
        let root = ctx.in_flight_mail_id();
        let forward_reply_to = ctx.reply_target();
        let DispatchTraced { mails } = batch;
        // Resolve every envelope's name addressing through the
        // substrate registry — same path `CaptureFrame`'s bundle
        // resolution uses (`render::on_capture_frame`). A single
        // unresolved name aborts the whole batch, surfaced as the
        // ack's `Err` variant so the MCP caller fails fast.
        let resolved = match resolve_bundle(&state.registry, &mails, "dispatch_traced batch") {
            Ok(v) => v,
            Err(error) => {
                return DispatchTracedAck::Err { error };
            }
        };
        for mail in resolved {
            let _ = ctx.send_envelope_tracked_with_reply_to(
                mail.recipient,
                mail.kind,
                mail.payload.bytes(),
                forward_reply_to,
            );
        }
        DispatchTracedAck::Ok { root }
    }
}

#[cfg(all(test, feature = "runtime"))]
// Tests hold the capture `Mutex` guard across the assertion block
// so the snapshot reads atomically against the concurrent push.
#[allow(clippy::significant_drop_tightening)]
mod tests {
    use super::*;
    use aether_data::{KindId, MailId, MailboxId, SessionToken, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::MailDispatch;
    use aether_substrate::mail::{Source, SourceAddr};
    use aether_substrate::testing::boot_authority;

    /// Shared scaffolding for the `on_dispatch_traced` tests:
    /// fresh registry + mailer + outbound + transport + state wired
    /// together. The state doesn't go through `init` (which reads ctx
    /// state); construct it directly with the registry handle the
    /// resolve path needs.
    struct DispatchTracedFixture {
        registry: Arc<Registry>,
        transport: Arc<NativeBinding>,
        state: TraceDispatchCapabilityState,
    }

    fn dispatch_traced_fixture() -> DispatchTracedFixture {
        let registry = Arc::new(Registry::new());
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x7ACE)));
        let state = TraceDispatchCapabilityState::with_registry(Arc::clone(&registry));
        DispatchTracedFixture { registry, transport, state }
    }

    /// Build a chassis-root `NativeCtx` against the fixture's
    /// transport, anchoring the in-flight + reply-to fields to a
    /// session sender so the ack reply egresses as `ToSession`.
    fn chassis_root_ctx(transport: &Arc<NativeBinding>, inbound: MailId) -> NativeCtx<'_> {
        let sender = Source::to(SourceAddr::Session(SessionToken(Uuid::nil())));
        NativeCtx::new(transport, sender, inbound, inbound)
    }

    /// Issue 749: `on_dispatch_traced` resolves each envelope's
    /// name addressing through the registry (matching
    /// `CaptureFrame`'s bundle pattern), dispatches each via
    /// `send_envelope_tracked` so children inherit the chain, and
    /// replies synchronously with `DispatchTracedAck::Ok { root }`
    /// carrying the inbound mail id.
    #[test]
    fn on_dispatch_traced_resolves_each_envelope_and_acks_with_root() {
        use aether_kinds::NamedMail;
        use std::sync::Mutex;

        type Capture = (KindId, MailId, Option<MailId>, Vec<u8>);

        /// Inline handler that records every dispatched mail's
        /// `(kind, root, parent, payload)` into the shared
        /// `Vec`. Used twice to register two stub recipients.
        fn register_capture(registry: &Registry, name: &str, sink: Arc<Mutex<Vec<Capture>>>) {
            registry.register_inline(
                &boot_authority(),
                name,
                Arc::new(move |d: MailDispatch<'_>| {
                    sink.lock().expect("test stub: captured mutex poisoned").push((
                        d.kind,
                        d.root,
                        d.parent_mail,
                        d.payload.to_vec(),
                    ));
                }),
            );
        }

        let mut fix = dispatch_traced_fixture();
        // Resolve_bundle needs both mailbox (by name) and kind to
        // be registered, else it short-circuits with the early-
        // abort `Err` path the other test exercises.
        let captured: Arc<Mutex<Vec<Capture>>> = Arc::new(Mutex::new(Vec::new()));
        register_capture(&fix.registry, "aether.test.spec_a", Arc::clone(&captured));
        register_capture(&fix.registry, "aether.test.spec_b", Arc::clone(&captured));
        let kind_alpha = fix.registry.register_kind(&boot_authority(), "aether.test.kind_a");
        let kind_beta = fix.registry.register_kind(&boot_authority(), "aether.test.kind_b");

        let inbound = MailId::new(MailboxId(0xC0DE), 7);
        let mut ctx = chassis_root_ctx(&fix.transport, inbound);
        let ack = TraceDispatchCapability::on_dispatch_traced(
            &mut fix.state,
            &mut ctx,
            DispatchTraced {
                mails: vec![
                    NamedMail {
                        recipient_name: "aether.test.spec_a".into(),
                        kind_name: "aether.test.kind_a".into(),
                        payload: vec![1u8, 2],
                        count: 1,
                    },
                    NamedMail {
                        recipient_name: "aether.test.spec_b".into(),
                        kind_name: "aether.test.kind_b".into(),
                        payload: vec![3u8, 4, 5],
                        count: 1,
                    },
                ],
            },
        );
        // 2b: the handler buffers its forwarded envelopes into the
        // actor's send-side ring; they route on handler-end flush.
        // Driving the handler directly (no dispatch loop), we drop the
        // ctx to trigger that flush — mirroring the per-envelope ctx
        // drop in `DispatcherSlot::dispatch_one` — before inspecting
        // the sink.
        drop(ctx);

        let snapshot = captured.lock().expect("test stub: captured mutex poisoned").clone();
        assert_eq!(snapshot.len(), 2, "expected each envelope to dispatch");
        assert!(
            snapshot.iter().any(|(k, root, parent, p)| *k == kind_alpha
                && *root == inbound
                && *parent == Some(inbound)
                && p == &vec![1u8, 2]),
            "envelope A missing or chain not inherited; captured: {snapshot:?}"
        );
        assert!(
            snapshot.iter().any(|(k, root, parent, p)| *k == kind_beta
                && *root == inbound
                && *parent == Some(inbound)
                && p == &vec![3u8, 4, 5]),
            "envelope B missing or chain not inherited; captured: {snapshot:?}"
        );

        match ack {
            DispatchTracedAck::Ok { root } => {
                assert_eq!(root, inbound, "Ok ack must echo the in-flight inbound mail id as the chassis root");
            }
            DispatchTracedAck::Err { error } => {
                panic!("expected Ok ack, got Err: {error}")
            }
        }
    }

    /// Issue 749: an unresolvable name in the batch short-circuits
    /// to `DispatchTracedAck::Err`; no envelope dispatches.
    #[test]
    fn on_dispatch_traced_replies_err_on_unknown_recipient() {
        use aether_kinds::NamedMail;

        let mut fix = dispatch_traced_fixture();
        let inbound = MailId::new(MailboxId(0xC0DE), 99);
        let mut ctx = chassis_root_ctx(&fix.transport, inbound);
        let ack = TraceDispatchCapability::on_dispatch_traced(
            &mut fix.state,
            &mut ctx,
            DispatchTraced {
                mails: vec![NamedMail {
                    recipient_name: "aether.test.does_not_exist".into(),
                    kind_name: "aether.test.also_missing".into(),
                    payload: vec![],
                    count: 1,
                }],
            },
        );

        // The batch is refused, and the refusal names both the offending
        // recipient and why it failed. Issue 4125 replaced a flat "unknown
        // recipient" with the structured `AddressResolutionError`, so an
        // ambiguous address is distinguishable from an absent one here rather
        // than collapsing to the same sentence.
        assert!(
            matches!(
                &ack,
                DispatchTracedAck::Err { error }
                    if error.contains("aether.test.does_not_exist") && error.contains("no live mailbox")
            ),
            "expected Err naming the absent recipient, got: {ack:?}"
        );
    }
}
