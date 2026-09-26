//! ADR-0231 §2, §6: a kind handled only by `#[fallback]` has no row, and a
//! `#[handler::manual]` handler's row is `Undeclared`, so neither covers a
//! single or a silent row. A protocol cannot name a manual row either:
//! `-> Undeclared` is not a `RowReply`.

use aether_actor::{
    ActorInitError, CoveredBy, Mail, Manual, Undeclared, WasmActor, WasmCtx, WasmInitCtx, actor, protocol,
};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_manual.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_manual.pong")]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_manual.note")]
struct Note {
    seq: u32,
}

#[protocol]
trait Pinger {
    fn ping(mail: Ping) -> Pong;
}

#[protocol]
trait Noter {
    fn note(mail: Note);
}

#[protocol]
trait ByHand {
    fn ping(mail: Ping) -> Undeclared;
}

struct FallbackOnly;

#[actor]
impl WasmActor for FallbackOnly {
    const NAMESPACE: &'static str = "test.protocol_manual.fallback_only";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_note(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Note) {}

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct ManualSingle;

#[actor]
impl WasmActor for ManualSingle {
    const NAMESPACE: &'static str = "test.protocol_manual.manual_single";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::manual]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_, Self, Manual>, _mail: Ping) {}
}

struct ManualSilent;

#[actor]
impl WasmActor for ManualSilent {
    const NAMESPACE: &'static str = "test.protocol_manual.manual_silent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::manual]
    fn on_note(&mut self, _ctx: &mut WasmCtx<'_, Self, Manual>, _mail: Note) {}
}

fn assert_covered<P: CoveredBy<R>, R>() {}

fn main() {
    assert_covered::<Pinger, FallbackOnly>();
    assert_covered::<Pinger, ManualSingle>();
    assert_covered::<Noter, ManualSilent>();
}
