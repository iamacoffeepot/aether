//! ADR-0147 fixture: a multi-actor module with an unconditional `boot =` slot.
//!
//! Exported via `export!(boot = Boot, WidgetA, WidgetB)`: `Boot` is the module's
//! boot actor — instantiated once per loaded module content hash, whatever
//! export selector a load names, and not itself selectable — while `WidgetA` /
//! `WidgetB` are ordinary selectable actors (no `default =`, so the module is
//! selector-load-only). The module carries an `aether.boot` custom section
//! naming `Boot`'s `NAMESPACE`, which the host reads to spawn and refcount the
//! boot singleton.
//!
//! `Boot` broadcasts observable markers to the `SubstrateHarness` observer mailbox so a
//! scenario can assert on the singleton's lifecycle with `count_observed`
//! (mirroring the `aether-test-fixtures-bundle` probe / `TickObserved`
//! pattern):
//!
//! - `wire` → [`BootObserved`], once per boot instance. Two selector loads of
//!   this module observe it exactly once — the module-boot singleton is
//!   instantiated once, not per load (cardinality).
//! - `unwire` → [`BootTornDown`], once when the host tears the boot down (its
//!   refcount reached zero as the last non-boot actor from the module unloaded).
//!   Stays at zero across a partial unload (boot survives), reaches one after
//!   the last unload (teardown).
//!
//! Kept standalone rather than folded into the shared bundle: an unconditional
//! boot slot on the bundle would spawn a boot on every one of its many
//! unrelated scenario loads.

// The `#[handler]` methods take `&mut self` to match the dispatch ABI even
// though these actors are stateless.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, MailSender, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Ping;
use aether_test_fixtures_kinds::{BootObserved, BootTornDown, SUBSTRATE_BENCH_OBSERVER_MAILBOX_NAME};

/// The module's unconditional boot actor (ADR-0147). Not selectable — a load
/// that names its `NAMESPACE` as the export selector is rejected by the host —
/// and instantiated exactly once per loaded module content hash.
pub struct Boot;

#[actor]
impl WasmActor for Boot {
    const NAMESPACE: &'static str = "aether.test.boot.boot";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Boot)
    }

    /// Broadcast [`BootObserved`] once, so a scenario counting it can assert the
    /// boot singleton was instantiated exactly once no matter how many selector
    /// loads of the module happened.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send_to_named::<BootObserved>(SUBSTRATE_BENCH_OBSERVER_MAILBOX_NAME, &BootObserved { marker: 0 });
    }

    /// Broadcast [`BootTornDown`] once when the host tears the boot down (its
    /// refcount reached zero). `unwire` is the trampoline's pre-shutdown hook,
    /// reached via the host's self-directed `DropComponent` at last unload.
    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send_to_named::<BootTornDown>(SUBSTRATE_BENCH_OBSERVER_MAILBOX_NAME, &BootTornDown { marker: 0 });
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

/// First ordinary selectable actor, reachable by its `NAMESPACE` export
/// selector. Refcounts against the module's boot singleton while loaded.
pub struct WidgetA;

#[actor]
impl WasmActor for WidgetA {
    const NAMESPACE: &'static str = "aether.test.boot.widget_a";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WidgetA)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

/// Second ordinary selectable actor, reachable by its `NAMESPACE` export
/// selector.
pub struct WidgetB;

#[actor]
impl WasmActor for WidgetB {
    const NAMESPACE: &'static str = "aether.test.boot.widget_b";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WidgetB)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

// ADR-0147: `Boot` is the unconditional boot slot; `WidgetA` / `WidgetB` are
// the ordinary selectable exports. No `default =` — this module is
// selector-load-only.
aether_actor::export!(boot = Boot, WidgetA, WidgetB);
