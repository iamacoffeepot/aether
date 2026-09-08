//! An `InlineChild<C>` carries the spawned child's type, so `send` is gated on
//! `C: HandlesKind<K>`: mailing a spawned child a kind it declares no
//! `#[handler]` for is a compile error rather than a substrate warn-drop (or,
//! for a child with a `#[fallback]`, silence).

use aether_actor::{ActorInitError, Mail, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.inline_child.handled")]
struct Handled {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.inline_child.also_handled")]
struct AlsoHandled {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.inline_child.unhandled")]
struct Unhandled {
    seq: u32,
}

struct Parent;

#[actor]
impl WasmActor for Parent {
    const NAMESPACE: &'static str = "test.inline_child.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Child;

#[actor(instanced, composable)]
impl WasmActor for Child {
    const NAMESPACE: &'static str = "test.inline_child.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_handled(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Handled) {}

    #[handler::single]
    fn on_also_handled(&mut self, _ctx: &mut WasmCtx<'_>, _mail: AlsoHandled) {}
}

fn send_to_child(ctx: &mut WasmCtx<'_>) {
    let Ok(child) = ctx.spawn_inline_child::<Parent, Child>(Subname::Named("slot"), &()) else {
        return;
    };
    child.send(ctx, &Handled { seq: 1 });
    child.send(ctx, &AlsoHandled { seq: 2 });
    child.send(ctx, &Unhandled { seq: 3 });
}

fn main() {}
