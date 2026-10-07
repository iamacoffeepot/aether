//! ADR-0231 §2: an actor covers a `#[protocol]` when it has a handler for each
//! row's kind replying exactly the row's reply; an extra handler and a
//! `#[fallback]` do not get in the way. This includes an explicit unchecked row:
//! it narrows as a reference and path, sends without a reply-handler bound,
//! and maps to `ReplyContract::Unchecked` in the same list the actor publishes.

use aether_actor::{
    ActorInitError, ActorPath, ActorRef, Anyone, CastTarget, Contracts, CoveredBy, Mail, Protocol, ProtocolPath,
    ProtocolRef, RowSet, Unchecked, Undeclared, WasmActor, WasmCtx, WasmInitCtx, actor, protocol,
};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_covered.load")]
struct Load {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_covered.loaded")]
struct Loaded {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_covered.set_mode")]
struct SetMode {
    mode: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_covered.extra")]
struct Extra {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_covered.forward")]
struct Forward {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_covered.trigger")]
struct Trigger;

/// Loads things.
#[protocol]
trait Loader {
    /// Load one thing.
    fn load(mail: Load) -> Loaded;
    fn set_mode(_: SetMode);
    fn forward(mail: Forward) -> Undeclared;
}

struct LoaderActor;

#[actor]
impl WasmActor for LoaderActor {
    const NAMESPACE: &'static str = "test.protocol_covered.loader";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::request]
    fn on_load(&mut self, _ctx: &mut WasmCtx<'_>, mail: Load) -> Loaded {
        Loaded { seq: mail.seq }
    }

    #[handler::tell]
    fn on_set_mode(&mut self, _ctx: &mut WasmCtx<'_>, _mail: SetMode) {}

    #[handler::tell]
    fn on_extra(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Extra) {}

    #[handler::unchecked(reason = "test: an explicit unchecked protocol row")]
    fn on_forward(&mut self, _ctx: &mut WasmCtx<'_, Self, Anyone, Unchecked>, _mail: Forward) {}

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

/// A sender with no handler for any reply the unchecked recipient might choose.
struct Sender;

#[actor]
impl WasmActor for Sender {
    const NAMESPACE: &'static str = "test.protocol_covered.sender";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_trigger(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Trigger) {}
}

fn narrow_and_send_unchecked(
    ctx: &mut WasmCtx<'_, Sender>,
    reference: ActorRef<LoaderActor>,
    path: &ActorPath<LoaderActor>,
) {
    let protocol: ProtocolRef<Loader> = reference.narrow::<Loader>();
    let _path: ProtocolPath<Loader> = path.narrow::<Loader>();

    ctx.send_to(protocol, &Forward { seq: 7 });
}

fn assert_covered<P: CoveredBy<R>, R>() {}

fn main() {
    assert_covered::<Loader, LoaderActor>();

    let protocol_rows = <<Loader as Protocol>::Rows as RowSet>::CONTRACTS;
    let actor_rows = <LoaderActor as Contracts>::CONTRACTS;
    assert_eq!(protocol_rows.len(), 3);
    for row in protocol_rows {
        assert!(actor_rows.contains(row), "protocol row {row:?} is missing from the covering actor's rows");
    }
    assert!(<Loader as CastTarget>::admits(actor_rows));
}
