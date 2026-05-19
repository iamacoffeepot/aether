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
use aether_kinds::{SetWindowMode, SetWindowTitle};

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;

    use aether_actor::actor;
    use aether_kinds::{SetWindowModeResult, SetWindowTitleResult};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::outbound::HubOutbound;

    use super::{SetWindowMode, SetWindowTitle};
    use std::io;

    /// Chassis-without-window companion to the desktop driver's
    /// driver-as-actor `aether.window` claim. Mirrors
    /// [`crate::HeadlessRenderCapability`]: same mailbox the desktop
    /// owner claims, `Err`-replying handlers so MCP `set_window_mode`
    /// / `set_window_title` fail fast on chassis without a window
    /// (headless and test-bench).
    ///
    /// Each chassis composes one of {desktop driver, this cap}, never
    /// both — the chassis builder rejects double-claiming a mailbox.
    pub struct HeadlessWindowCapability {
        outbound: Arc<HubOutbound>,
    }

    #[actor]
    impl NativeActor for HeadlessWindowCapability {
        type Config = ();

        const NAMESPACE: &'static str = "aether.window";

        fn init(_config: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
                BootError::Other(Box::new(io::Error::other(
                    "HubOutbound must be wired on Mailer before \
                     HeadlessWindowCapability::init (chassis main connects the hub before \
                     the Builder chain)",
                )))
            })?;
            Ok(Self { outbound })
        }

        /// Reply `Err` so MCP `set_window_mode` fails fast instead of
        /// hanging on a reply that never comes.
        #[handler]
        fn on_set_mode(&self, ctx: &mut NativeCtx<'_>, _mail: SetWindowMode) {
            self.outbound.send_reply(
                ctx.reply_target(),
                &SetWindowModeResult::Err {
                    error: "unsupported on this chassis — no window peripheral".to_owned(),
                },
            );
        }

        /// Reply `Err` for the same reason as `on_set_mode`.
        #[handler]
        fn on_set_title(&self, ctx: &mut NativeCtx<'_>, _mail: SetWindowTitle) {
            self.outbound.send_reply(
                ctx.reply_target(),
                &SetWindowTitleResult::Err {
                    error: "unsupported on this chassis — no window peripheral".to_owned(),
                },
            );
        }
    }
}
