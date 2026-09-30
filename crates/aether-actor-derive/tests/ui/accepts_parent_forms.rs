//! Issue 7206: `ctx.parent()` takes the shape of the placement. A child-only
//! actor sends through it directly, and an actor that is also `root` matches
//! the `Option` it returns. Either send compiles because every declared parent
//! handles the kind.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.parent_forms.report")]
struct Report {
    seq: u32,
}

struct Panel;

#[actor(root)]
impl WasmActor for Panel {
    const NAMESPACE: &'static str = "test.parent_forms.panel";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_report(&mut self, _ctx: &mut WasmCtx<'_>, _report: Report) {}
}

struct Scroll;

#[actor(instanced, root, child_of(Panel))]
impl WasmActor for Scroll {
    const NAMESPACE: &'static str = "test.parent_forms.scroll";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_report(&mut self, ctx: &mut WasmCtx<'_>, report: Report) {
        if let Some(parent) = ctx.parent() {
            parent.send(&report);
        }
    }
}

struct Slider;

#[actor(instanced, child_of(Panel, Scroll))]
impl WasmActor for Slider {
    const NAMESPACE: &'static str = "test.parent_forms.slider";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_report(&mut self, ctx: &mut WasmCtx<'_>, report: Report) {
        ctx.parent().send(&report);
    }
}

fn main() {}
