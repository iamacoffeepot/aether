//! `aether.window` cap surface (issue 603 Phase 3).
//!
//! On desktop the chassis driver claims `aether.window` directly and
//! drains the inbox between frames — window mutations require the
//! chassis main thread (winit / macOS), and the driver is already
//! there. The driver-as-actor path lives in
//! `crate::desktop::driver`; this module hosts the chassis-without-window
//! companion that headless and test-bench compose to fail-fast with
//! `Err`-replies on `set_mode` / `set_title`.

// Handler-signature kinds must be importable at module root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers always-on,
// outside the `feature = "runtime"` gate.
use aether_kinds::{FocusWindow, SetWindowMode, SetWindowTitle};

use aether_actor::actor;

/// `aether.window` headless-companion cap **identity** (ADR-0122
/// identity/runtime split). A ZST carrying only the addressing — the
/// `Addressable` / `HandlesKind` markers and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`HeadlessWindowCapabilityState`) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names it
/// nor pulls `aether_substrate` through this cap.
///
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

// The reply kinds ride the native gate (not `runtime`): the `#[actor]`
// macro's ADR-0109 `HandlerEntry` inventory submission — emitted on every
// native build, runtime or not — names each handler's reply kind `::ID`,
// so a transport-only build must still see them. The `aether_substrate`-
// typed ctx imports and the empty state struct sit behind the one
// `feature = "runtime"` gate; the macro gates everything it emits for the
// runtime half, so this cap's identity compiles transport-only.
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::{FocusWindowResult, SetWindowModeResult, SetWindowTitleResult};

#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

/// The `aether.window` headless-companion runtime half (ADR-0122 split):
/// the `aether_substrate`-typed ctx imports and the state struct, gated once
/// by this module rather than per-import.
#[cfg(feature = "runtime")]
mod runtime {
    pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    pub use aether_substrate::chassis::error::BootError;

    /// `aether.window` headless-companion runtime state (ADR-0122 split).
    /// The cap is stateless — every handler `Err`-replies off `ctx` alone —
    /// so this is a named empty struct standing in for future state rather
    /// than `()` or `Self`. The addressing identity is the distinct ZST
    /// [`HeadlessWindowCapability`](super::HeadlessWindowCapability).
    pub struct HeadlessWindowCapabilityState;
}

#[actor(singleton)]
impl NativeActor for HeadlessWindowCapability {
    /// The runtime state this identity boots into (ADR-0122 split): a
    /// named empty struct, the stateless cap's stand-in for future state.
    type State = HeadlessWindowCapabilityState;

    type Config = ();

    const NAMESPACE: &'static str = "aether.window";

    fn init(
        _config: (),
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<HeadlessWindowCapabilityState, BootError> {
        Ok(HeadlessWindowCapabilityState)
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
    #[handler]
    fn on_set_mode(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SetWindowMode,
    ) -> SetWindowModeResult {
        SetWindowModeResult::Err {
            error: "unsupported on this chassis — no window peripheral".to_owned(),
        }
    }

    /// Reply `Err` for the same reason as `on_set_mode`.
    #[handler]
    fn on_set_title(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SetWindowTitle,
    ) -> SetWindowTitleResult {
        SetWindowTitleResult::Err {
            error: "unsupported on this chassis — no window peripheral".to_owned(),
        }
    }

    /// Reply `Err` for the same reason as `on_set_mode`
    /// (iamacoffeepot/aether#1318): a chassis without a window
    /// peripheral can't foreground one.
    #[handler]
    fn on_focus(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: FocusWindow,
    ) -> FocusWindowResult {
        FocusWindowResult::Err {
            error: "unsupported on this chassis — no window peripheral".to_owned(),
        }
    }
}

#[cfg(all(test, feature = "runtime"))]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod tests {
    use super::*;
    use aether_data::{Kind, MailId, Source, SourceAddr};
    use aether_kinds::{FocusWindow, SetWindowMode, SetWindowTitle, WindowMode};
    use aether_substrate::actor::native::Dispatch;
    use aether_substrate::actor::native::binding::NativeBinding;
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
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
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
    /// unchained path. Each handler is a single-class `-> R` handler;
    /// the macro-emitted dispatch issues the reply so they all share the
    /// proof via the dispatch trampoline (ADR-0112).
    #[test]
    fn err_reply_joins_caller_chain() {
        let (mailer, caller_mailbox, reply_rx) = settlement_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        // ADR-0122 split: the dispatch trampoline routes over the runtime
        // state, not the identity.
        let mut state = HeadlessWindowCapabilityState;

        // Run each handler under its own root through the dispatch
        // trampoline (ADR-0112: `NativeCtx::new_dispatching` + `__aether_dispatch_envelope`)
        // and assert the reply holds, then settles, that root.
        let mut next_correlation = 0_u64;
        let mut drive = |kind_id: aether_data::KindId, payload: &[u8]| {
            next_correlation += 1;
            let root = MailId::new(MailboxId(0x1710), next_correlation);
            let caller_source =
                Source::with_correlation(SourceAddr::Component(caller_mailbox), next_correlation);

            {
                let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
                <HeadlessWindowCapability as Dispatch<HeadlessWindowCapabilityState>>::dispatch(
                    &mut state, &mut ctx, kind_id, payload,
                );
            }

            assert_eq!(
                counter.live_roots(),
                1,
                "the macro-emitted reply holds the caller chain open",
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

        drive(
            SetWindowMode::ID,
            &SetWindowMode {
                mode: WindowMode::Windowed,
                width: None,
                height: None,
            }
            .encode_into_bytes(),
        );
        drive(
            SetWindowTitle::ID,
            &SetWindowTitle {
                title: "test".to_owned(),
            }
            .encode_into_bytes(),
        );
        drive(FocusWindow::ID, &FocusWindow {}.encode_into_bytes());
    }
}
