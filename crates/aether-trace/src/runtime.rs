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
    #[handler::request]
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
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;

    use super::*;
    use aether_data::{ErasedActorPath, KindId, MailId, SessionToken, Uuid};
    use aether_kinds::NamedMail;
    use aether_substrate::PumpedSlot;
    use aether_substrate::chassis::builder::{PassiveChassis, ReplyTarget};
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::mail::registry::{MailDispatch, Registry};
    use aether_substrate::testing::{
        TestChassis, boot_authority, boot_bare_test_chassis, decode_session_reply, fresh_substrate_and_rx,
    };

    /// One mail a stub recipient received: `(kind, root, parent, payload)`.
    type Capture = (KindId, Option<MailId>, Option<MailId>, Vec<u8>);

    /// A real `TraceDispatchCapability` booted as a pumped actor. A batch is
    /// pushed as a tracked chassis-root mail, so it carries the root the
    /// RPC bridge stamps in production, and
    /// [`PumpedSlot::drain_available`] runs the production dispatch body,
    /// whose context drop flushes the forwarded children.
    struct PumpedTrace {
        rx: Receiver<EgressEvent>,
        /// Declared before `cap` so it drops first: a chassis goes before
        /// the pumped root it hosts, which closes when its slot drops.
        chassis: PassiveChassis<TestChassis>,
        cap: PumpedSlot<TraceDispatchCapability>,
    }

    impl PumpedTrace {
        /// Boot the cap after `seed` has stood the batch's recipients and
        /// kinds in the registry, returning what `seed` returned beside it.
        fn boot<T>(seed: impl FnOnce(&Registry) -> T) -> (Self, T) {
            let (registry, mailer, rx) = fresh_substrate_and_rx();
            let seeded = seed(&registry);
            let chassis = boot_bare_test_chassis(&registry, &mailer);
            let (cap, _wake) = chassis
                .boot_pumped_actor::<TraceDispatchCapability>((), ())
                .expect("TraceDispatchCapability boots pumped");

            (Self { rx, chassis, cap }, seeded)
        }

        /// Dispatch `mails` and return the batch's minted root beside the
        /// decoded ack.
        fn dispatch(&mut self, mails: Vec<NamedMail>) -> (MailId, DispatchTracedAck) {
            let (root, _settled) = self.chassis.send_tracked(
                self.chassis.actor_ref::<TraceDispatchCapability>(),
                &DispatchTraced { mails },
                Some(ReplyTarget::Session { session: SessionToken(Uuid::nil()), correlation: 1 }),
            );
            self.cap.drain_available();

            (root, decode_session_reply(&self.rx))
        }
    }

    /// Register an inline stub recipient under `name` that records every
    /// mail it receives on `sink`.
    fn register_capture(registry: &Registry, name: &str, sink: Sender<Capture>) {
        registry.register_inline(
            &boot_authority(),
            name,
            Arc::new(move |d: MailDispatch<'_>| {
                let _ = sink.send((d.kind, d.root, d.parent_mail, d.payload.to_vec()));
            }),
        );
    }

    fn named_mail(recipient: &str, kind_name: &str, payload: Vec<u8>) -> NamedMail {
        NamedMail {
            recipient: ErasedActorPath::new(recipient).expect("a well-formed actor path"),
            kind_name: kind_name.into(),
            payload,
            count: 1,
        }
    }

    /// Issue 749. Bug caught: a child dispatched on a fresh chain rather
    /// than inheriting the batch's, so the tree walked from the ack's `root`
    /// misses it; an ack whose `root` is not the batch's own id; or a spec
    /// dropped or mis-routed.
    #[test]
    fn dispatch_traced_children_descend_from_the_root_the_ack_reports() {
        let (sink, captured) = mpsc::channel();
        let (mut trace, (kind_alpha, kind_beta)) = PumpedTrace::boot(|registry| {
            register_capture(registry, "aether.test.spec_a", sink.clone());
            register_capture(registry, "aether.test.spec_b", sink);
            (
                registry.register_kind(&boot_authority(), "aether.test.kind_a"),
                registry.register_kind(&boot_authority(), "aether.test.kind_b"),
            )
        });

        let (batch_root, ack) = trace.dispatch(vec![
            named_mail("aether.test.spec_a", "aether.test.kind_a", vec![1, 2]),
            named_mail("aether.test.spec_b", "aether.test.kind_b", vec![3, 4, 5]),
        ]);

        let DispatchTracedAck::Ok { root } = ack else {
            panic!("expected Ok ack, got {ack:?}");
        };
        assert_eq!(root, batch_root, "the ack reports the batch's own id as the root");

        let mut children: Vec<Capture> = (0..2)
            .map(|_| captured.recv_timeout(Duration::from_secs(2)).expect("each spec reaches its recipient"))
            .collect();
        children.sort_by_key(|(_, _, _, payload)| payload.len());
        assert_eq!(
            children,
            vec![(kind_alpha, Some(root), Some(root), vec![1, 2]), (kind_beta, Some(root), Some(root), vec![3, 4, 5]),],
            "each child carries its spec's kind and payload and descends from the acked root",
        );
    }

    /// Issue 749. Bug caught: an unresolvable recipient dispatched past, or
    /// refused without naming which recipient failed and why.
    #[test]
    fn dispatch_traced_refuses_a_batch_with_an_unknown_recipient() {
        let (mut trace, ()) = PumpedTrace::boot(|_| {});

        let (_, ack) =
            trace.dispatch(vec![named_mail("aether.test.does_not_exist", "aether.test.also_missing", vec![])]);

        // Issue 4125 replaced a flat "unknown recipient" with the structured
        // `AddressResolutionError`, so an ambiguous address is
        // distinguishable from an absent one here.
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
