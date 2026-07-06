//! ADR-0134: a `#[handler::multi]` handler must carry a `Multi<K>` ctx
//! marker naming the emit kind. A ctx without it — here `WasmCtx<'_>`
//! (= the default `Single` view) — earns a pointed macro error naming the
//! required `Multi<K>` shape, not an opaque unification failure at the
//! generated call site.

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

struct MismatchProbe;

#[actor]
impl aether_actor::WasmActor for MismatchProbe {
    const NAMESPACE: &'static str = "multi_mismatch_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(MismatchProbe)
    }

    // multi class but a single-mode ctx — the macro cannot read `K` off a
    // `Multi<K>` marker that isn't there.
    #[handler::multi]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
