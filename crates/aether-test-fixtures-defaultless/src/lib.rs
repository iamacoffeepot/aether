//! ADR-0138 fixture: a defaultless multi-actor module.
//!
//! Two `WasmActor` types, `Alpha` and `Beta`, exported via a bare
//! `export!(Alpha, Beta)` with no `default =`. Per ADR-0138 that designates
//! **no** bare-load default: the module carries the `aether.no_default` marker
//! section and omits `aether.namespace`, so the host rejects a `load` with
//! no export selector (a hard error naming the exports) while a named
//! `export: Some("test.defaultless.alpha")` / `…beta` load resolves through
//! the ADR-0096 typed-init path exactly as before.
//!
//! Kept out of the main `aether-test-fixtures-bundle` (which opts into a
//! `Probe` default via `export!(default = …)`) so the bundle's default-load
//! scenarios stay intact; this crate exists only to exercise the
//! no-default load path.

// The `#[handler]` methods take `&mut self` to match the dispatch ABI even
// though these actors are stateless.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Ping;

/// First exported type. Not a bare-load default — a defaultless module has
/// no default, so it is reachable only by its `NAMESPACE` export selector.
pub struct Alpha;

#[actor]
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

#[actor]
impl WasmActor for Beta {
    const NAMESPACE: &'static str = "test.defaultless.beta";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Beta)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

// ADR-0138: a bare `export!(A, B)` (no `default =`) is deliberately
// defaultless — this is the whole point of the fixture.
aether_actor::export!(Alpha, Beta);
