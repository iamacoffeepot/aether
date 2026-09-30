//! ADR-0231 §2: a target covers a row only with the exact reply type. A handler
//! replying a different kind, a silent handler for a single row, and a single
//! handler for a silent row each leave the protocol uncovered.

use aether_actor::{ActorInitError, CoveredBy, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_mismatch.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_mismatch.pong")]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_mismatch.other")]
struct Other {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.protocol_mismatch.note")]
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

struct WrongReply;

#[actor]
impl WasmActor for WrongReply {
    const NAMESPACE: &'static str = "test.protocol_mismatch.wrong_reply";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::request]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, mail: Ping) -> Other {
        Other { seq: mail.seq }
    }
}

struct SilentForSingle;

#[actor]
impl WasmActor for SilentForSingle {
    const NAMESPACE: &'static str = "test.protocol_mismatch.silent_for_single";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Ping) {}
}

struct SingleForSilent;

#[actor]
impl WasmActor for SingleForSilent {
    const NAMESPACE: &'static str = "test.protocol_mismatch.single_for_silent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::request]
    fn on_note(&mut self, _ctx: &mut WasmCtx<'_>, mail: Note) -> Pong {
        Pong { seq: mail.seq }
    }
}

fn assert_covered<P: CoveredBy<R>, R>() {}

fn main() {
    assert_covered::<Pinger, WrongReply>();
    assert_covered::<Pinger, SilentForSingle>();
    assert_covered::<Noter, SingleForSilent>();
}
