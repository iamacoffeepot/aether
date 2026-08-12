//! iamacoffeepot/aether#4811: a `#[cfg]` on a `#[handler]` governs every
//! artifact the wasm expansion derives from it.
//!
//! `syn` does not evaluate `cfg`, so the expansion sees all three handlers here
//! regardless of the configuration it is expanded in. This fixture is compiled
//! for the host, which strips `on_only_on_wasm` and its `WasmOnly` kind while
//! keeping `on_only_off_wasm` and `on_always`. Both directions have to hold in
//! the same compile:
//!
//!   - the stripped handler must contribute no `HandlesKind` impl, no dispatch
//!     arm, no `aether.kinds.inputs` record, and no kind-retention static — each
//!     names `WasmOnly` and `on_only_on_wasm`, neither of which exists here, so
//!     leaking any one of them fails the build with `cannot find type` /
//!     `no associated function`;
//!   - the surviving handlers must still produce all four, so the gate is not
//!     stripping more than it was asked to.
//!
//! A component crate really is built both ways — for `wasm32-unknown-unknown`
//! and for the host when its tests run — so this is the shape an author hits,
//! not a contrivance.

use aether_actor::actor;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.cfg_gated.always")]
struct Always {
    seq: u32,
}

#[cfg(target_family = "wasm")]
#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.cfg_gated.only_on_wasm")]
struct WasmOnly {
    seq: u32,
}

#[cfg(not(target_family = "wasm"))]
#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.cfg_gated.only_off_wasm")]
struct HostOnly {
    seq: u32,
}

struct CfgGated;

#[actor]
impl aether_actor::WasmActor for CfgGated {
    const NAMESPACE: &'static str = "test.cfg_gated";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(CfgGated)
    }

    #[handler::single]
    fn on_always(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: Always) {}

    #[handler::single]
    #[cfg(target_family = "wasm")]
    fn on_only_on_wasm(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: WasmOnly) {}

    #[handler::single]
    #[cfg(not(target_family = "wasm"))]
    fn on_only_off_wasm(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: HostOnly) {}
}

fn main() {
    // The surviving handlers keep their `HandlesKind` markers; the stripped one
    // has none to check. Naming them here is what proves the gate removed only
    // the artifacts of the handler it was written on.
    fn handles<K: aether_data::Kind, A: aether_actor::HandlesKind<K>>() {}
    handles::<Always, CfgGated>();
    handles::<HostOnly, CfgGated>();
}
