//! ADR-0231 §3: `ActorRef<R>::narrow::<P>()` and its spawn-result sibling
//! `InlineChild<C>::narrow::<P>()` compile only for `P: CoveredBy<_>`. `Child`
//! handles `Handled` but not `Unhandled`, so it covers `Covered` and not
//! `Uncovered`; a bound loosened to `P: Protocol` would let a holder keep a
//! reference that sends `Unhandled` to an actor that only warn-drops it.

use aether_actor::{ActorInitError, ActorRef, InlineChild, Mail, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reference_narrow.handled")]
struct Handled {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reference_narrow.unhandled")]
struct Unhandled {
    seq: u32,
}

#[protocol]
trait Covered {
    fn handled(mail: Handled);
}

#[protocol]
trait Uncovered {
    fn handled(mail: Handled);
    fn unhandled(mail: Unhandled);
}

struct Parent;

#[actor]
impl WasmActor for Parent {
    const NAMESPACE: &'static str = "test.reference_narrow.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Child;

#[actor(instanced, child_of(Parent))]
impl WasmActor for Child {
    const NAMESPACE: &'static str = "test.reference_narrow.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_handled(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Handled) {}
}

fn narrow_reference(reference: ActorRef<Child>) {
    let _covered = reference.narrow::<Covered>();
    let _uncovered = reference.narrow::<Uncovered>();
}

fn narrow_spawned(child: InlineChild<Child>) {
    let _covered = child.narrow::<Covered>();
    let _uncovered = child.narrow::<Uncovered>();
}

fn main() {}
