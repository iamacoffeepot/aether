//! ADR-0112: the class marker and the ctx marker must agree — the macro
//! passes the manual view to a `#[handler::manual]` and the single view
//! to a `#[handler]`, so a signature whose ctx marker disagrees fails to
//! unify. Two mismatches on the wasm path:
//!   - manual class + `WasmCtx<'_>` (= Single) ctx,
//!   - single class + `WasmCtx<'_, Manual>` ctx.

use aether_actor::{WasmCtx, Manual, actor};

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
#[kind(name = "test.pong")]
struct Pong {
    seq: u32,
}

struct MismatchProbe;

#[actor]
impl aether_actor::WasmActor for MismatchProbe {
    const NAMESPACE: &'static str = "mismatch_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(MismatchProbe)
    }

    // manual class but a single-mode ctx — the macro passes the `Manual`
    // ctx, which doesn't unify with `WasmCtx<'_>`.
    #[handler::manual]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}

    // single class but a manual-mode ctx — the macro passes `as_single()`,
    // which doesn't unify with `WasmCtx<'_, Manual>`.
    #[handler]
    fn on_pong(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _pong: Pong) {}
}

fn main() {}
