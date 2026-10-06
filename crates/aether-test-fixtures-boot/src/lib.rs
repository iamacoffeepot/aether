//! ADR-0147 fixture: a multi-actor module with an unconditional `boot =` slot.
//!
//! Exported via `export!(boot = Boot, public = [WidgetA, WidgetB])`: `Boot` is the module's
//! boot actor — instantiated once per loaded module content hash, whatever
//! export selector a load names, and not itself selectable — while `WidgetA` /
//! `WidgetB` are ordinary selectable actors, so the module is
//! selector-load-only for them. The module carries an `aether.boot` custom section
//! naming `Boot`'s `NAMESPACE`, which the host reads to spawn the boot
//! singleton once, by the module's first load. A module that declares a boot
//! is not replaceable.
//!
//! `Boot` broadcasts observable markers to the `SubstrateHarness` observer mailbox so a
//! scenario can assert on the singleton's lifecycle with `count_observed`
//! (mirroring the `aether-test-fixtures-bundle` probe / `TickObserved`
//! pattern):
//!
//! - `wire` → [`BootObserved`], once per boot instance. Two selector loads of
//!   this module observe it exactly once — the module-boot singleton is
//!   instantiated once, not per load (cardinality).
//! - `unwire` → [`BootTornDown`], once when the boot closes: on a drop
//!   addressed at it, or when its engine tears down. Stays at zero while
//!   every widget unloads (the boot outlives them), and reaches one after the
//!   boot's own close.
//!
//! Kept standalone rather than folded into the shared bundle: an unconditional
//! boot slot on the bundle would spawn a boot for its many unrelated scenario
//! loads and make the bundle unreplaceable.

#![forbid(unsafe_code)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Ping;
use aether_test_fixtures_kinds::{BootObserved, BootTornDown, SubstrateHarnessObserver};

/// The module's unconditional boot actor (ADR-0147). Not selectable — a load
/// that names its `NAMESPACE` as the export selector is rejected by the host —
/// and instantiated exactly once per loaded module content hash.
pub struct Boot;

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for Boot {
    const NAMESPACE: &'static str = "aether.test.boot.boot";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Boot)
    }

    /// Broadcast [`BootObserved`] once, so a scenario counting it can assert the
    /// boot singleton was instantiated exactly once no matter how many selector
    /// loads of the module happened.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.send::<SubstrateHarnessObserver>(&BootObserved { marker: 0 });
    }

    /// Broadcast [`BootTornDown`] once when the boot closes. `unwire` runs in
    /// every close of the boot's trampoline: a `DropComponent` addressed at
    /// the boot, or its engine's teardown.
    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send::<SubstrateHarnessObserver>(&BootTornDown { marker: 0 });
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

/// First ordinary selectable actor, reachable by its `NAMESPACE` export
/// selector.
pub struct WidgetA;

#[actor(root)]
impl WasmActor for WidgetA {
    const NAMESPACE: &'static str = "aether.test.boot.widget_a";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WidgetA)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

/// Second ordinary selectable actor, reachable by its `NAMESPACE` export
/// selector.
pub struct WidgetB;

#[actor(root)]
impl WasmActor for WidgetB {
    const NAMESPACE: &'static str = "aether.test.boot.widget_b";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WidgetB)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

// ADR-0147: `Boot` is the unconditional boot slot; `WidgetA` / `WidgetB` are
// the ordinary selectable exports — this module is selector-load-only for
// them.
aether_actor::export!(boot = Boot, public = [WidgetA, WidgetB]);
