//! iamacoffeepot/aether#4341: a `capture_frame` the render cap rejects has
//! to answer over the wire.
//!
//! The reply edge is the whole bug, and only the wire shows it. An RPC
//! `Call` reaches a cap with the rpc server's own mailbox as its reply
//! target — a `SourceAddr::Component` — while the failure paths answered
//! through `HubOutbound`, which routes `Session` / `EngineMailbox` senders
//! and drops the rest. So a rejected capture returned no image, no error
//! and no timeout, and the cap's own unit tests passed throughout because
//! they drove the session shape, the one branch the broken edge served.
//!
//! `FleetHarness` rather than `SubstrateHarness` per the harness decision
//! rule: the assertion is that a reply crosses the hub's RPC boundary, not
//! that anything renders.

mod tests {
    use aether_data::Kind;
    use aether_kinds::{CaptureFrame, CaptureFrameResult, WindowId};

    use aether_harness_fleet::FleetHarness;

    /// A headless engine has no GPU, so every `capture_frame` against one
    /// is rejected — which makes it the cheapest possible probe of the
    /// reply edge, with no window, adapter or drawn frame in the way. The
    /// assertion is that *some* `CaptureFrameResult` comes back at all:
    /// before the fix this call drew zero reply envelopes.
    #[test]
    fn a_rejected_capture_replies_over_the_wire() {
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        let replies = harness.send(
            engine,
            "aether.render",
            &CaptureFrame {
                window: Some(WindowId(42)),
                mails: Vec::new(),
                after_mails: Vec::new(),
                checks: Vec::new(),
                similarity: None,
            },
        );

        let reply = replies
            .iter()
            .find(|envelope| envelope.kind == <CaptureFrameResult as Kind>::ID)
            .expect("a rejected capture replies rather than falling silent");
        let decoded =
            CaptureFrameResult::decode_from_bytes(&reply.payload).expect("the reply decodes as a capture result");
        assert!(
            matches!(decoded, CaptureFrameResult::Err { .. }),
            "a headless engine has no capture target, so the reply is an Err naming why",
        );
    }
}
