//! The `aether.trace` runtime half (ADR-0122 identity/runtime split):
//! the `aether_substrate`-typed imports and the `#[runtime] impl`, gated
//! once by this module rather than per-import.

use super::{DispatchTraced, TraceDispatchCapability};
use aether_actor::runtime;

#[cfg(not(target_family = "wasm"))]
use super::DispatchTracedAck;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

#[runtime]
impl NativeActor for TraceDispatchCapability {
    /// The runtime state this identity boots into (ADR-0122 split): none.
    /// Every recipient is proven through `ctx.accept_bundle` at receipt.
    type State = ();

    type Config = ();
    // `aether.trace` (matches
    // `aether_kinds::trace::TRACE_MAILBOX_NAME`). Has to be a literal
    // here for the `#[actor]` macro's expansion.
    const NAMESPACE: &'static str = "aether.trace";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<(), BootError> {
        Ok(())
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
    /// **Reply forwarding (issue 1265).** Each child is delivered
    /// through `ctx.deliver_forwarded`, which pins its reply target to
    /// this `DispatchTraced`'s own (the caller — typically the RPC
    /// server holding the wire `cid`'s in-flight entry) rather than
    /// to this cap. This trace cap is a re-dispatcher with no handler
    /// for child reply kinds, so without the forward each child's
    /// deferred reply (the ADR-0093 hold-until-resolve dispatch in
    /// content-gen caps) lands here and silently drops, leaving the
    /// wire call with no `ReplyEvent`s. Forwarding lets every child's
    /// reply (sync or deferred) bubble straight to the original caller
    /// with the same `correlation_id`, so the RPC server's `on_any`
    /// fallback wraps each into a `ReplyEvent` on the wire.
    #[handler::single]
    fn on_dispatch_traced(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        batch: DispatchTraced,
    ) -> DispatchTracedAck {
        // The RPC bridge always stamps the batch, so its own id is the root
        // every child descends from; a batch without one has no root to ack.
        let Some(root) = ctx.in_flight_mail_id() else {
            return DispatchTracedAck::Err { error: "dispatch_traced arrived without a causal chain".to_owned() };
        };
        // Prove every recipient before any child moves (ADR-0230 §3). A
        // single unprovable recipient or unknown kind aborts the whole
        // batch, surfaced as the ack's `Err` variant so the MCP caller
        // fails fast.
        let batch = match ctx.accept_bundle(batch.mails, "dispatch_traced batch") {
            Ok(items) => items,
            Err(error) => return DispatchTracedAck::Err { error },
        };
        for item in batch {
            ctx.deliver_forwarded(item);
        }
        DispatchTracedAck::Ok { root }
    }
}

#[cfg(all(test, feature = "runtime"))]
// Tests hold the capture `Mutex` guard across the assertion block
// so the snapshot reads atomically against the concurrent push.
#[allow(clippy::significant_drop_tightening)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use aether_data::{KindId, MailId, SessionToken, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::{MailDispatch, Registry};
    use aether_substrate::mail::{Source, SourceAddr};
    use aether_substrate::testing::{boot_authority, token_root, unrouted_binding};

    /// Shared scaffolding for the `on_dispatch_traced` tests:
    /// fresh registry + mailer + outbound + transport wired together.
    /// The registry registers the stub recipients and kinds the
    /// bundle proof reads.
    struct DispatchTracedFixture {
        registry: Arc<Registry>,
        transport: Arc<NativeBinding>,
    }

    fn dispatch_traced_fixture() -> DispatchTracedFixture {
        let registry = Arc::new(Registry::new());
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        let transport = unrouted_binding(&mailer);
        DispatchTracedFixture { registry, transport }
    }

    /// Build a chassis-root `NativeCtx` against the fixture's
    /// transport, anchoring the in-flight + reply-to fields to a
    /// session sender so the ack reply egresses as `ToSession`.
    fn chassis_root_ctx<A>(transport: &Arc<NativeBinding>, inbound: MailId) -> NativeCtx<'_, A> {
        let sender = Source::to(SourceAddr::Session(SessionToken(Uuid::nil())));
        NativeCtx::new_for_actor(transport, sender, Some(inbound), Some(inbound))
    }

    /// Issue 749: `on_dispatch_traced` proves each envelope's
    /// recipient through `accept_bundle` (matching `CaptureFrame`'s
    /// bundle pattern), delivers each via `deliver_forwarded` so
    /// children inherit the chain, and
    /// replies synchronously with `DispatchTracedAck::Ok { root }`
    /// carrying the inbound mail id.
    #[test]
    fn on_dispatch_traced_resolves_each_envelope_and_acks_with_root() {
        use aether_data::ActorPath;
        use aether_kinds::NamedMail;
        use std::sync::Mutex;

        type Capture = (KindId, Option<MailId>, Option<MailId>, Vec<u8>);

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

        let fix = dispatch_traced_fixture();
        // The bundle proof needs both mailbox (by name) and kind to
        // be registered, else it short-circuits with the early-
        // abort `Err` path the other test exercises.
        let captured: Arc<Mutex<Vec<Capture>>> = Arc::new(Mutex::new(Vec::new()));
        register_capture(&fix.registry, "aether.test.spec_a", Arc::clone(&captured));
        register_capture(&fix.registry, "aether.test.spec_b", Arc::clone(&captured));
        let kind_alpha = fix.registry.register_kind(&boot_authority(), "aether.test.kind_a");
        let kind_beta = fix.registry.register_kind(&boot_authority(), "aether.test.kind_b");

        let inbound = token_root(7);
        let mut ctx = chassis_root_ctx(&fix.transport, inbound);
        let ack = TraceDispatchCapability::on_dispatch_traced(
            &mut (),
            &mut ctx,
            DispatchTraced {
                mails: vec![
                    NamedMail {
                        recipient: ActorPath::new("aether.test.spec_a").expect("a well-formed actor path"),
                        kind_name: "aether.test.kind_a".into(),
                        payload: vec![1u8, 2],
                        count: 1,
                    },
                    NamedMail {
                        recipient: ActorPath::new("aether.test.spec_b").expect("a well-formed actor path"),
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
                && *root == Some(inbound)
                && *parent == Some(inbound)
                && p == &vec![1u8, 2]),
            "envelope A missing or chain not inherited; captured: {snapshot:?}"
        );
        assert!(
            snapshot.iter().any(|(k, root, parent, p)| *k == kind_beta
                && *root == Some(inbound)
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
        use aether_data::ActorPath;
        use aether_kinds::NamedMail;

        let fix = dispatch_traced_fixture();
        let inbound = token_root(99);
        let mut ctx = chassis_root_ctx(&fix.transport, inbound);
        let ack = TraceDispatchCapability::on_dispatch_traced(
            &mut (),
            &mut ctx,
            DispatchTraced {
                mails: vec![NamedMail {
                    recipient: ActorPath::new("aether.test.does_not_exist").expect("a well-formed actor path"),
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
