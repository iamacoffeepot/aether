//! ADR-0134: a `#[handler::multi]` FFI handler receives the `Multi<K>`
//! ctx and answers one dispatch with 0..n mails of the declared kind `K`
//! via `Emit::emit` — the multi-class path compiles cleanly on the wasm
//! expansion, and the macro reads `K` off the `Multi<K>` signature.

use aether_actor::{Emit, Multi, WasmCtx, actor};

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

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.frame")]
struct Frame {
    n: u32,
}

struct MultiProbe;

#[actor]
impl aether_actor::WasmActor for MultiProbe {
    const NAMESPACE: &'static str = "multi_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(MultiProbe)
    }

    #[handler::multi]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_, Multi<Frame>>, ping: Ping) {
        for n in 0..ping.seq {
            ctx.emit(&Frame { n });
        }
    }
}

fn main() {}
