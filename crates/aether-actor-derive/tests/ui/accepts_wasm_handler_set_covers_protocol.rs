//! ADR-0231 §2 / §10: a wasm `#[handler_set]` carries its rows to an adopter
//! through its marker bridge, so the adopter covers a protocol that lists a set
//! kind, with the set's reply, beside a kind the adopter handles itself.
//!
//! Every `Contract<K>` row names its position in the adopter's one `Rows`
//! list, and that position is checked against the list, so a bridge that
//! omitted a set row, placed one at the wrong position, or left the set's rows
//! off the list's tail fails to compile here. The adopter sits in a module
//! other than the set's, so the bridge must be reachable through the set path
//! the adopter names.

use aether_actor::{Contracts, CoveredBy, Protocol, RowSet, protocol};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.wasm_set_covers.frame")]
pub struct Frame {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.wasm_set_covers.ask")]
pub struct Ask {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.wasm_set_covers.answer")]
pub struct Answer {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.wasm_set_covers.local")]
pub struct Local {
    seq: u32,
}

mod set {
    use aether_actor::{WasmCtx, handler_set};

    #[handler_set]
    pub trait Shared {
        fn seen(&mut self) -> &mut u32;

        #[handler::tell]
        fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: crate::Frame) {
            *self.seen() = frame.seq;
        }

        #[handler::request]
        fn on_ask(&mut self, _ctx: &mut WasmCtx<'_>, ask: crate::Ask) -> crate::Answer {
            crate::Answer { seq: ask.seq }
        }
    }
}

mod adopter {
    use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

    use crate::Local;
    use crate::set::Shared;

    pub struct Adopter {
        seen: u32,
    }

    impl Shared for Adopter {
        fn seen(&mut self) -> &mut u32 {
            &mut self.seen
        }
    }

    #[actor(handler_set(Shared))]
    impl WasmActor for Adopter {
        const NAMESPACE: &'static str = "test.wasm_set_covers.adopter";

        fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            Ok(Adopter { seen: 0 })
        }

        #[handler::tell]
        fn on_local(&mut self, _ctx: &mut WasmCtx<'_>, local: Local) {
            self.seen = local.seq;
        }
    }
}

/// A local row and both set rows, the second of them a replying one.
#[protocol]
trait Lane {
    fn local(mail: Local);
    fn frame(mail: Frame);
    fn ask(mail: Ask) -> Answer;
}

fn assert_covered<P: CoveredBy<R>, R>() {}

fn main() {
    assert_covered::<Lane, adopter::Adopter>();

    let actor_rows = <adopter::Adopter as Contracts>::CONTRACTS;
    for row in <<Lane as Protocol>::Rows as RowSet>::CONTRACTS {
        assert!(actor_rows.contains(row), "protocol row {row:?} is missing from the adopter's rows");
    }
}
