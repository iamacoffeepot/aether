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
    use aether_actor::actor;
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
        /// Reply through `ctx.mailer().send_reply` (the `Mailer`, the
        /// complete router) rather than `HubOutbound::send_reply`, which
        /// silently drops `ReplyTarget::Component` — the local-RPC-server
        /// reply target an MCP-spawned engine tags (iamacoffeepot/aether#1321,
        /// matching the desktop driver fix in #1319).
        // `&self` keeps the dispatch ABI (ADR-0033 / ADR-0038); the
        // capability is stateless, so the reply routes purely off `ctx`.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_set_mode(&self, ctx: &mut NativeCtx<'_>, _mail: SetWindowMode) {
            ctx.mailer().send_reply(
                ctx.reply_target(),
                &SetWindowModeResult::Err {
                    error: "unsupported on this chassis — no window peripheral".to_owned(),
                },
            );
        }

        /// Reply `Err` for the same reason as `on_set_mode`.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_set_title(&self, ctx: &mut NativeCtx<'_>, _mail: SetWindowTitle) {
            ctx.mailer().send_reply(
                ctx.reply_target(),
                &SetWindowTitleResult::Err {
                    error: "unsupported on this chassis — no window peripheral".to_owned(),
                },
            );
        }

        /// Reply `Err` for the same reason as `on_set_mode`
        /// (iamacoffeepot/aether#1318): a chassis without a window
        /// peripheral can't foreground one.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_focus(&self, ctx: &mut NativeCtx<'_>, _mail: FocusWindow) {
            ctx.mailer().send_reply(
                ctx.reply_target(),
                &FocusWindowResult::Err {
                    error: "unsupported on this chassis — no window peripheral".to_owned(),
                },
            );
        }
    }
}
