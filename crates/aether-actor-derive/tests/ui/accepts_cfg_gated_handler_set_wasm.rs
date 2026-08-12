//! ADR-0183, wasm half: a `#[cfg]` on a `#[handler_set]` handler gates every
//! artifact the set derives from it, and the crate defining the set is the one
//! whose configuration decides.
//!
//! A wasm set emits two artifact families — the dispatch chain's arms and the
//! `aether.kinds.inputs` manifest records — and no marker bridge, so this
//! fixture covers the whole of what a wasm set produces. The bridge belongs to
//! native sets, and its gate pair is asserted over the emitted tokens in
//! `handler_set::tests`: a native set's expansion names `aether_substrate`
//! types, and `aether-substrate` depends transitively on the crate under test.
//! A dev-dependency can close that cycle, so what rules a fixture out here is
//! cost, not the dependency graph: it would pull the wasmtime and cranelift tree
//! into these UI tests, and hand-mocking the substrate types to avoid that proves
//! less than the token assertion does. The real types are in scope on the
//! substrate side of the edge if the pair is wanted against them.
//!
//! trybuild compiles a fixture as a plain binary, so `test` is off here:
//! `on_test_only` is stripped and `on_not_test` survives. Both directions are
//! checked, and neither check can pass by accident.
//!
//! **Stripped.** `TestOnly` carries the same `#[cfg(test)]` as the handler that
//! receives it, so the kind type does not exist in this configuration. An arm or
//! a manifest record that leaked past the gate would name it, and the fixture
//! would not compile.
//!
//! **Surviving.** `Surviving` is the same set with the two handlers that outlive
//! `#[cfg(test)]` declared plainly, so its manifest is the exact byte sequence
//! `Shared`'s must reduce to. The const assertion below compares the two, which
//! fails if `Shared` lost a record it should have kept as readily as if it kept
//! one it should have lost — a gate that stripped everything is caught here, and
//! nothing about the comparison holds if the `#[cfg]`s are dropped on the floor
//! the way they were before ADR-0183. The dispatch arms ride the same collected
//! attribute list as the records, so pinning the records pins which handlers the
//! set believes it owns.
//!
//! **Stripped to nothing.** `AllStripped` is a set whose only handler is gated
//! off here — the empty end of the range, newly reachable now that a set's
//! `#[cfg]`s survive collection. Its manifest writer runs with every one of its
//! length and copy statements removed, and the assertion below pins the result
//! at no records rather than one empty one.

use aether_actor::{actor, handler_set};

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.cfg_set_wasm.always")]
struct Always {
    seq: u32,
}

#[cfg(test)]
#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.cfg_set_wasm.test_only")]
struct TestOnly {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.cfg_set_wasm.not_test")]
struct NotTest {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.cfg_set_wasm.local")]
struct Local {
    seq: u32,
}

#[handler_set]
trait Shared {
    fn seen(&mut self) -> &mut u32;

    #[handler::single]
    fn on_always(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, always: Always) {
        *self.seen() += always.seq;
    }

    #[handler::single]
    #[cfg(test)]
    fn on_test_only(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, test_only: TestOnly) {
        *self.seen() += test_only.seq;
    }

    #[handler::single]
    #[cfg(not(test))]
    fn on_not_test(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, not_test: NotTest) {
        *self.seen() += not_test.seq;
    }
}

/// The surface `Shared` must reduce to when `test` is off, written without a
/// single `#[cfg]` so it is a fixed reference rather than a second reading of
/// the machinery under test.
#[handler_set]
trait Surviving {
    fn seen(&mut self) -> &mut u32;

    #[handler::single]
    fn on_always(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, always: Always) {
        *self.seen() += always.seq;
    }

    #[handler::single]
    fn on_not_test(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, not_test: NotTest) {
        *self.seen() += not_test.seq;
    }
}

/// Every handler gated off in this configuration. Nothing survives to compare
/// against a reference, so what this set pins is the degenerate case: a set can
/// still be declared, adopted, and asked for its manifest when the answer is
/// nothing at all.
#[handler_set]
trait AllStripped {
    fn seen(&mut self) -> &mut u32;

    #[handler::single]
    #[cfg(test)]
    fn on_test_only(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, test_only: TestOnly) {
        *self.seen() += test_only.seq;
    }
}

struct Adopter {
    seen: u32,
}

impl Shared for Adopter {
    fn seen(&mut self) -> &mut u32 {
        &mut self.seen
    }
}

struct Reference {
    seen: u32,
}

impl Surviving for Reference {
    fn seen(&mut self) -> &mut u32 {
        &mut self.seen
    }
}

struct Stripped {
    seen: u32,
}

impl AllStripped for Stripped {
    fn seen(&mut self) -> &mut u32 {
        &mut self.seen
    }
}

#[actor(handler_set(Shared))]
impl aether_actor::WasmActor for Adopter {
    const NAMESPACE: &'static str = "test.cfg_set_wasm.adopter";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(Adopter { seen: 0 })
    }

    #[handler::single]
    fn on_local(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, local: Local) {
        self.seen = local.seq;
    }
}

const fn same_bytes(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

const _: () = assert!(same_bytes(
    <Adopter as Shared>::__AETHER_HANDLER_SET_MANIFEST,
    <Reference as Surviving>::__AETHER_HANDLER_SET_MANIFEST,
));

// The boundary the comparison above cannot express: a set with nothing left
// carries no records, not one empty record and not the record of the handler it
// was told to strip.
const _: () = assert!(<Stripped as AllStripped>::__AETHER_HANDLER_SET_MANIFEST.is_empty());

fn main() {
    // Both witness types exist to name their set's manifest in the assertions
    // above, which are compile-time and hold whether or not either is ever
    // built. Constructing them is what puts each set's required accessor — the
    // method its handler bodies reach through — into the compile as well.
    let mut reference = Reference { seen: 0 };
    let mut stripped = Stripped { seen: 0 };
    *reference.seen() += 1;
    *stripped.seen() += 1;
}
