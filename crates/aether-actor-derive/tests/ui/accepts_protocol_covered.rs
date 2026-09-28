//! ADR-0231 §2: an actor covers a `#[protocol]` when it has a handler for each
//! row's kind replying exactly the row's reply; an extra handler and a
//! `#[fallback]` do not get in the way. `RowSet::CONTRACTS` must use the mapping
//! `#[actor]`'s `Contracts::CONTRACTS` uses (a silent row is
//! `ReplyContract::None`), because a route's published rows are compared with
//! it, so every protocol row appears in the covering actor's list, and the
//! guard cast's protocol arm (ADR-0231 §4), which `#[protocol]` opts into,
//! admits those published rows.

use aether_actor::{
    ActorInitError, CastTarget, Contracts, CoveredBy, Mail, Protocol, RowSet, WasmActor, WasmCtx, WasmInitCtx, actor, protocol,
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

/// Loads things.
#[protocol]
trait Loader {
    /// Load one thing.
    fn load(mail: Load) -> Loaded;
    fn set_mode(_: SetMode);
}

struct LoaderActor;

#[actor]
impl WasmActor for LoaderActor {
    const NAMESPACE: &'static str = "test.protocol_covered.loader";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_load(&mut self, _ctx: &mut WasmCtx<'_>, mail: Load) -> Loaded {
        Loaded { seq: mail.seq }
    }

    #[handler::single]
    fn on_set_mode(&mut self, _ctx: &mut WasmCtx<'_>, _mail: SetMode) {}

    #[handler::single]
    fn on_extra(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Extra) {}

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn assert_covered<P: CoveredBy<R>, R>() {}

fn main() {
    assert_covered::<Loader, LoaderActor>();

    let protocol_rows = <<Loader as Protocol>::Rows as RowSet>::CONTRACTS;
    let actor_rows = <LoaderActor as Contracts>::CONTRACTS;
    assert_eq!(protocol_rows.len(), 2);
    for row in protocol_rows {
        assert!(actor_rows.contains(row), "protocol row {row:?} is missing from the covering actor's rows");
    }
    assert!(<Loader as CastTarget>::admits(actor_rows));
}
