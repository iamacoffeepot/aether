//! Issue 7206: a send through `ctx.parent()` compiles only for a kind every
//! declared parent handles. `Child` lists `Handles` and `Ignores`, and only
//! `Handles` has a handler for `Report`.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.parent_unhandled.report")]
struct Report {
    seq: u32,
}

struct Handles;

#[actor(root)]
impl WasmActor for Handles {
    const NAMESPACE: &'static str = "test.parent_unhandled.handles";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_report(&mut self, _ctx: &mut WasmCtx<'_>, _report: Report) {}
}

struct Ignores;

#[actor(root)]
impl WasmActor for Ignores {
    const NAMESPACE: &'static str = "test.parent_unhandled.ignores";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Child;

#[actor(instanced, child_of(Handles, Ignores))]
impl WasmActor for Child {
    const NAMESPACE: &'static str = "test.parent_unhandled.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_report(&mut self, ctx: &mut WasmCtx<'_>, report: Report) {
        ctx.parent().send(&report);
    }
}

fn main() {}
