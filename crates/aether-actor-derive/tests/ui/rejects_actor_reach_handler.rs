//! A handler's kind must cross actors (ADR-0242). A kind holding a
//! `ReplyHandle`, a per-instance host handle of actor reach, is a request
//! context and never mail, so a handler for it fails to compile.

use aether_actor::{ReplyHandle, actor};

#[derive(Clone, Debug, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reach.load_context")]
struct LoadContext {
    reply: ReplyHandle,
    label: String,
}

struct Loader;

#[actor]
impl aether_actor::WasmActor for Loader {
    const NAMESPACE: &'static str = "loader";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Loader)
    }

    #[handler::tell]
    fn on_context(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _context: LoadContext) {}
}

fn main() {}
