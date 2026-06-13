//! `aether.window` cap surface (issue 603 Phase 3).
//!
//! On desktop the chassis driver claims `aether.window` directly and
//! drains the inbox between frames — window mutations require the
//! chassis main thread (winit / macOS), and the driver is already
//! there. The driver-as-actor path lives in
//! `crate::desktop::driver`; this module hosts the chassis-without-window
//! companion that headless and test-bench compose to fail-fast with
//! `Err`-replies on `set_mode` / `set_title`.

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{FocusWindow, SetWindowMode, SetWindowTitle};

#[aether_actor::bridge(singleton)]
mod native {
    use aether_actor::{OutboundReply, actor};
    use aether_kinds::{FocusWindowResult, SetWindowModeResult, SetWindowTitleResult};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use super::{FocusWindow, SetWindowMode, SetWindowTitle};

    /// Chassis-without-window companion to the desktop driver's
    /// driver-as-actor `aether.window` claim. Mirrors
    /// [`crate::HeadlessRenderCapability`]: same mailbox the desktop
    /// owner claims, `Err`-replying handlers so MCP `set_window_mode`
    /// / `set_window_title` fail fast on chassis without a window
    /// (headless and test-bench).
    ///
    /// Each chassis composes one of {desktop driver, this cap}, never
    /// both — the chassis builder rejects double-claiming a mailbox.
    pub struct HeadlessWindowCapability;

    #[actor]
    impl NativeActor for HeadlessWindowCapability {
        type Config = ();

        const NAMESPACE: &'static str = "aether.window";

        fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        /// Reply `Err` so MCP `set_window_mode` fails fast instead of
        /// hanging on a reply that never comes.
        ///
        /// Reply through the typed `ctx.reply()` (the
        /// `NativeBinding::send_reply_for_handler` path), which mints the
        /// reply id and joins the caller's ADR-0080 causal chain so the
        /// blocking `set_window_mode` settles on the reply's `Finished`.
        /// It routes every `SourceAddr` — including the `Component`
        /// local-RPC-server reply target an MCP-spawned engine tags
        /// (iamacoffeepot/aether#1321) that `HubOutbound::send_reply`
        /// silently drops.
        // `&self` keeps the dispatch ABI (ADR-0033 / ADR-0038); the
        // capability is stateless, so the reply routes purely off `ctx`.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_set_mode(&self, ctx: &mut NativeCtx<'_>, _mail: SetWindowMode) {
            ctx.reply(&SetWindowModeResult::Err {
                error: "unsupported on this chassis — no window peripheral".to_owned(),
            });
        }

        /// Reply `Err` for the same reason as `on_set_mode`.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_set_title(&self, ctx: &mut NativeCtx<'_>, _mail: SetWindowTitle) {
            ctx.reply(&SetWindowTitleResult::Err {
                error: "unsupported on this chassis — no window peripheral".to_owned(),
            });
        }

        /// Reply `Err` for the same reason as `on_set_mode`
        /// (iamacoffeepot/aether#1318): a chassis without a window
        /// peripheral can't foreground one.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_focus(&self, ctx: &mut NativeCtx<'_>, _mail: FocusWindow) {
            ctx.reply(&FocusWindowResult::Err {
                error: "unsupported on this chassis — no window peripheral".to_owned(),
            });
        }
    }

    #[cfg(test)]
    #[allow(
        clippy::unwrap_used,
        reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
    )]
    mod tests {
        use super::*;
        use aether_data::{MailId, Source, SourceAddr};
        use aether_kinds::{FocusWindow, SetWindowMode, SetWindowTitle, WindowMode};
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::{HubOutbound, InboxHandler, MailboxId, OwnedDispatch, Registry};
        use std::sync::Arc;
        use std::sync::mpsc;
        use std::time::Duration;

        /// `(mailer, caller_mailbox, reply_rx)` — a mailer with a
        /// `Component`-target caller inbox that discharges each delivered
        /// reply and forwards the dispatch so the test can read its
        /// lineage. Mirrors the audio cap's `settlement_substrate`.
        fn settlement_substrate() -> (Arc<Mailer>, MailboxId, mpsc::Receiver<OwnedDispatch>) {
            let registry = Arc::new(Registry::new());
            let (outbound, _egress_rx) = HubOutbound::attached_loopback();
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer =
                Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
            let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
            let caller_mailbox = registry.register_inbox(
                "test.window.settlement.caller",
                Arc::new(move |dispatch: OwnedDispatch| {
                    // ADR-0094: terminal consumer — discharge before forwarding.
                    dispatch.discharge();
                    let _ = reply_tx.send(dispatch);
                }) as Arc<dyn InboxHandler>,
            );
            (mailer, caller_mailbox, reply_rx)
        }

        /// #1701 / #1710 conformance: a blocking `set_mode` / `set_title` /
        /// `focus` reply from the headless window cap joins the caller's
        /// causal chain — the reply's `Sent` holds the root open until its
        /// `Finished` fires, instead of detaching through the lineage-less
        /// unchained path. Each handler routes through the typed
        /// `ctx.reply()`, so they all share the proof.
        #[test]
        fn err_reply_joins_caller_chain() {
            let (mailer, caller_mailbox, reply_rx) = settlement_substrate();
            let counter = Arc::clone(mailer.trace_handle().settlement_counter());
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let cap = HeadlessWindowCapability;

            // Run each handler under its own root and assert the reply
            // holds, then settles, that root.
            let mut next_correlation = 0_u64;
            let mut drive = |handler: &dyn Fn(&HeadlessWindowCapability, &mut NativeCtx<'_>)| {
                next_correlation += 1;
                let root = MailId::new(MailboxId(0x1710), next_correlation);
                let caller_source = Source::with_correlation(
                    SourceAddr::Component(caller_mailbox),
                    next_correlation,
                );

                {
                    let mut ctx = NativeCtx::new(&transport, caller_source, root, root);
                    handler(&cap, &mut ctx);
                }

                assert_eq!(
                    counter.live_roots(),
                    1,
                    "the in-handler reply holds the caller chain open",
                );
                let dispatch = reply_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("Err reply reached the caller inbox");
                assert_eq!(dispatch.root, root, "reply inherits the caller's root");
                mailer.record_finished(dispatch.mail_id, dispatch.root);
                assert_eq!(
                    counter.live_roots(),
                    0,
                    "chain settles after the reply's Finished fires",
                );
            };

            drive(&|cap, ctx| {
                cap.on_set_mode(
                    ctx,
                    SetWindowMode {
                        mode: WindowMode::Windowed,
                        width: None,
                        height: None,
                    },
                );
            });
            drive(&|cap, ctx| {
                cap.on_set_title(
                    ctx,
                    SetWindowTitle {
                        title: "test".to_owned(),
                    },
                );
            });
            drive(&|cap, ctx| {
                cap.on_focus(ctx, FocusWindow {});
            });
        }
    }
}
