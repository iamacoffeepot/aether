//! ADR-0112: the class marker and the ctx marker must agree — the macro
//! passes the unchecked view to a `#[handler::unchecked(..)]` and the single view
//! to a `#[handler]`, so a signature whose ctx marker disagrees fails to
//! unify. Two mismatches on the wasm path:
//!   - unchecked class + `WasmCtx<'_>` (= Single) ctx,
//!   - single class + `WasmCtx<'_, Erased, Unchecked>` ctx.

use aether_actor::{Erased, Unchecked, WasmCtx, actor};

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

    // unchecked class but a single-mode ctx — the macro passes the `Unchecked`
    // ctx, which doesn't unify with `WasmCtx<'_>`.
    #[handler::unchecked(reason = "test: the class and ctx marker disagree")]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}

    // single class but an unchecked-mode ctx — the macro passes `as_single()`,
    // which doesn't unify with `WasmCtx<'_, Erased, Unchecked>`.
    #[handler::single]
    fn on_pong(&mut self, _ctx: &mut WasmCtx<'_, Erased, Unchecked>, _pong: Pong) {}
}

fn main() {}
