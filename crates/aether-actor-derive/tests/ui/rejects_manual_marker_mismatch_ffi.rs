//! ADR-0112: the class marker and the ctx marker must agree — the macro
//! passes the manual view to a `#[handler::manual]` and the single view
//! to a `#[handler]`, so a signature whose ctx marker disagrees fails to
//! unify. Two mismatches on the wasm path:
//!   - manual class + `FfiCtx<'_>` (= Single) ctx,
//!   - single class + `FfiCtx<'_, Manual>` ctx.

use aether_actor::{FfiCtx, Manual, actor};

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
impl aether_actor::FfiActor for MismatchProbe {
    const NAMESPACE: &'static str = "mismatch_probe";

    fn init<C>(_ctx: &mut C) -> Result<Self, aether_actor::BootError>
    where
        C: aether_actor::Resolver,
    {
        Ok(MismatchProbe)
    }

    // manual class but a single-mode ctx — the macro passes the `Manual`
    // ctx, which doesn't unify with `FfiCtx<'_>`.
    #[handler::manual]
    fn on_ping(&mut self, _ctx: &mut FfiCtx<'_>, _ping: Ping) {}

    // single class but a manual-mode ctx — the macro passes `as_single()`,
    // which doesn't unify with `FfiCtx<'_, Manual>`.
    #[handler]
    fn on_pong(&mut self, _ctx: &mut FfiCtx<'_, Manual>, _pong: Pong) {}
}

fn main() {}
