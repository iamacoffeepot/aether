//! A small multi-actor module with no bare-load entry (ADR-0241 §9).
//!
//! Two `WasmActor` types, `Alpha` and `Beta`, exported via
//! `export!(public = [Alpha, Beta])`. With two non-boot exports, the host
//! rejects a `load` with no export selector (a hard error naming the exports)
//! while a named `export: Some("test.defaultless.alpha")` / `…beta` load
//! resolves through the ADR-0096 typed-init path.
//!
//! Kept out of the main `aether-test-fixtures-bundle` so its two-type shape
//! stays minimal; this crate exists only to exercise the unselected-load
//! refusal.

#![forbid(unsafe_code)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Ping;

/// First exported type. Not a bare-load entry — the module exports two types,
/// so each is reachable only by its `NAMESPACE` export selector.
pub struct Alpha;

#[actor(root)]
impl WasmActor for Alpha {
    const NAMESPACE: &'static str = "test.defaultless.alpha";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Alpha)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

/// Second exported type, selectable by its `NAMESPACE`.
pub struct Beta;

#[actor(root)]
impl WasmActor for Beta {
    const NAMESPACE: &'static str = "test.defaultless.beta";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Beta)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

// Two non-boot exports, so no unselected load resolves — the whole point of
// the fixture.
aether_actor::export!(public = [Alpha, Beta]);
