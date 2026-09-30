//! ADR-0231 §3: a request whose kind carries a `ProtocolPath` is answered when
//! the path does not prove at decode, so its reply must be able to name the
//! refusal. A handler of such a request whose reply is not `From<PathRefused>`
//! fails to compile.

use aether_actor::{ActorInitError, ProtocolPath, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[aether_data::kind(name = "test.path_refusal.poke", copy)]
struct Poke;

#[protocol]
trait Poking {
    fn poke(mail: Poke);
}

#[aether_data::kind(name = "test.path_refusal.register", no_serde)]
struct Register {
    target: ProtocolPath<Poking>,
}

#[aether_data::kind(name = "test.path_refusal.registered", copy)]
struct Registered;

struct Registry;

#[actor]
impl WasmActor for Registry {
    const NAMESPACE: &'static str = "test.path_refusal.registry";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Registry)
    }

    #[handler::request]
    fn on_register(&mut self, _ctx: &mut WasmCtx<'_>, _register: Register) -> Registered {
        Registered
    }
}

fn main() {}
