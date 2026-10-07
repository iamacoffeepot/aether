//! ADR-0231 §2, §6: a kind handled only by `#[fallback]` has no row, and a
//! `#[handler::unchecked(..)]` handler's `Undeclared` row covers only an explicit
//! unchecked protocol row. It does not cover a single or silent row, and an
//! unchecked protocol reference still sends only the kind it lists.

use aether_actor::{
    ActorInitError, Anyone, CoveredBy, Mail, ProtocolRef, Unchecked, Undeclared, WasmActor, WasmCtx, WasmInitCtx, actor,
    protocol,
};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_unchecked.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_unchecked.pong")]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_unchecked.note")]
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
    const NAMESPACE: &'static str = "test.protocol_unchecked.fallback_only";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_note(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Note) {}

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct UncheckedSingle;

#[actor]
impl WasmActor for UncheckedSingle {
    const NAMESPACE: &'static str = "test.protocol_unchecked.unchecked_single";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked(reason = "test: an unchecked row covering a single row")]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_, Self, Anyone, Unchecked>, _mail: Ping) {}
}

struct UncheckedSilent;

#[actor]
impl WasmActor for UncheckedSilent {
    const NAMESPACE: &'static str = "test.protocol_unchecked.unchecked_silent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked(reason = "test: an unchecked row covering a silent row")]
    fn on_note(&mut self, _ctx: &mut WasmCtx<'_, Self, Anyone, Unchecked>, _mail: Note) {}
}

fn assert_covered<P: CoveredBy<R>, R>() {}

fn send_unsupported(ctx: &mut WasmCtx<'_, UncheckedSingle>, target: ProtocolRef<ByHand>) {
    ctx.send_to(target, &Note { seq: 1 });
}

fn main() {
    assert_covered::<Pinger, FallbackOnly>();
    assert_covered::<ByHand, FallbackOnly>();
    assert_covered::<Pinger, UncheckedSingle>();
    assert_covered::<Noter, UncheckedSilent>();
}
