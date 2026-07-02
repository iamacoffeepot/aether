//! Issue #2460: a `&[K]` slice/batch handler is native-only — the wasm
//! dispatcher decodes a single `K` per mail, so `impl HandlesKind<&[K]>`
//! / `decode_kind::<&[K]>()` would be emitted and fail to compile. The
//! macro rejects the slice shape at the boundary with a pointed message
//! instead of letting the opaque codegen error surface.

use aether_actor::{WasmCtx, actor};

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

struct SliceProbe;

#[actor]
impl aether_actor::WasmActor for SliceProbe {
    const NAMESPACE: &'static str = "slice_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(SliceProbe)
    }

    #[handler]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _mail: &[Ping]) {}
}

fn main() {}
