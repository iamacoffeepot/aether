//! iamacoffeepot/aether#4811, native half: a `#[cfg]` on a `#[handler]` governs
//! the always-on addressing markers too.
//!
//! This crate has no `aether-substrate` dev-dependency, so a native fixture can
//! only compile the surface that does not name substrate types — which is
//! exactly the always-on half: the `Addressable` impl, one `HandlesKind<K>` per
//! handler, the name-inventory entry, and the ADR-0109 §5 `HandlerEntry`
//! submissions. The split shape plus `runtime_feature` (borrowed from
//! `accepts_actor_runtime_feature.rs`) cfgs the substrate-typed runtime impls
//! out; `accepts_cfg_gated_handler_wasm.rs` and the
//! `a_cfg_gated_handler_leaves_no_dispatch_artifact` test in
//! `aether-substrate/tests/native_actor_macro.rs` carry the dispatch, capability,
//! and measured-kind halves.
//!
//! `on_gated` and its `Gated` kind are stripped here, so the marker impl and the
//! inventory row derived from them must be stripped with them — each names a
//! type and a namespace that do not exist in this configuration.

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
#[kind(name = "test.cfg_native.always")]
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
#[kind(name = "test.cfg_native.gated")]
struct Gated {
    seq: u32,
}

pub struct CfgNativeCap;

struct CfgNativeCapState {
    seen: u32,
}

#[actor(singleton, runtime_feature = "gated-native")]
impl aether_substrate::actor::native::NativeActor for CfgNativeCap {
    type State = CfgNativeCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.cfg_native_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<CfgNativeCapState, aether_substrate::chassis::error::BootError> {
        Ok(CfgNativeCapState { seen: 0 })
    }

    #[handler::single]
    fn on_always(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _mail: Always,
    ) {
        state.seen += 1;
    }

    #[handler::single]
    #[cfg(target_family = "wasm")]
    fn on_gated(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _mail: Gated,
    ) {
        state.seen += 1;
    }
}

fn main() {
    // The ungated handler keeps its marker; the gated one has none to name.
    fn handles<K: aether_data::Kind, A: aether_actor::HandlesKind<K>>() {}
    handles::<Always, CfgNativeCap>();

    // The runtime impls that would construct the state are gated out by the
    // absent `gated-native` feature, so name it here rather than suppressing the
    // dead-code lint the fixture would otherwise trip.
    let state = CfgNativeCapState { seen: 0 };
    assert_eq!(state.seen, 0);
}
