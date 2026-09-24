//! ADR-0112 amendment (#6412): the reply handle belongs to the manual reply
//! surface. A single-class dispatch returns `DISPATCH_HANDLED_RELEASE` and
//! the substrate frees its handle when the handler returns, so a single
//! handler that kept the handle would answer nothing. `reply_target` lives
//! only on the `Manual` ctx, so reading it from a `#[handler::single]` body
//! is a compile error. A handler that keeps its handle declares
//! `#[handler::manual]`.

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

struct HandleKeeper;

#[actor]
impl aether_actor::WasmActor for HandleKeeper {
    const NAMESPACE: &'static str = "handle_keeper";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(HandleKeeper)
    }

    #[handler::single]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_>, _ping: Ping) {
        // The `Single` ctx has no `reply_target`, so this fails to compile.
        let _kept = ctx.reply_target();
    }
}

fn main() {}
