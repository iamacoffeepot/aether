//! Issue 7206: an actor that declares no `child_of(..)` has no parent door,
//! so `ctx.parent()` does not compile on it.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.parent_root_only.report")]
struct Report {
    seq: u32,
}

struct Lonely;

#[actor(root)]
impl WasmActor for Lonely {
    const NAMESPACE: &'static str = "test.parent_root_only.lonely";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_report(&mut self, ctx: &mut WasmCtx<'_>, _report: Report) {
        let _ = ctx.parent();
    }
}

fn main() {}
