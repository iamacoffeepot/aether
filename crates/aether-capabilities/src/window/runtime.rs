//! The `aether.window` headless-companion runtime half (ADR-0122 split):
//! the `aether_substrate`-typed ctx imports and the state struct, gated once
//! by this module rather than per-import. The `#[actor] impl` reaches them
//! through the single `use runtime::*` glob in the parent.

use super::{FocusWindow, HeadlessWindowCapability, SetWindowMode, SetWindowTitle};
use aether_actor::runtime;

#[cfg(not(target_family = "wasm"))]
use super::{FocusWindowResult, SetWindowModeResult, SetWindowTitleResult};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// `aether.window` headless-companion runtime state (ADR-0122 split).
/// The cap is stateless — every handler `Err`-replies off `ctx` alone —
/// so this is a named empty struct standing in for future state rather
/// than `()` or `Self`. The addressing identity is the distinct ZST
/// [`HeadlessWindowCapability`](super::HeadlessWindowCapability).
pub struct HeadlessWindowCapabilityState;

#[runtime]
impl NativeActor for HeadlessWindowCapability {
    /// The runtime state this identity boots into (ADR-0122 split): a
    /// named empty struct, the stateless cap's stand-in for future state.
    type State = HeadlessWindowCapabilityState;

    type Config = ();

    const NAMESPACE: &'static str = "aether.window";

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeadlessWindowCapabilityState, BootError> {
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
    #[handler::single]
    fn on_set_mode(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: SetWindowMode) -> SetWindowModeResult {
        SetWindowModeResult::Err { error: "unsupported on this chassis — no window peripheral".to_owned() }
    }

    /// Reply `Err` for the same reason as `on_set_mode`.
    #[handler::single]
    fn on_set_title(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: SetWindowTitle) -> SetWindowTitleResult {
        SetWindowTitleResult::Err { error: "unsupported on this chassis — no window peripheral".to_owned() }
    }

    /// Reply `Err` for the same reason as `on_set_mode`
    /// (iamacoffeepot/aether#1318): a chassis without a window
    /// peripheral can't foreground one.
    #[handler::single]
    fn on_focus(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: FocusWindow) -> FocusWindowResult {
        FocusWindowResult::Err { error: "unsupported on this chassis — no window peripheral".to_owned() }
    }
}
