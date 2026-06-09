//! Issue 1472 DAG `Source` fixture. A computation-DAG `Source` node
//! dispatches a `Mat4SourceTrigger` to this component; the handler
//! replies a fixed, hand-computable `Mat4Apply` operand. The reply
//! resolves the source node's handle and becomes the `mat4_apply`
//! transform's input downstream (`Source → Transform`).
//!
//! No production cap emits a transform-input kind, so this first-party
//! fixture is the only producer that lets a wire-driven
//! `Source → Transform → Observer` DAG validate against the forked
//! headless binary. It also exercises the cast `Mat4Apply` wire
//! round-trip: the reply's cast encode must decode at the transform
//! input.
//!
//! The baked matrix is the column-major scale(2,3,4) + translate(5,6,7),
//! applied to `(1,1,1,1)` — `M·v = (7,9,11,1)`, clean integers with
//! exact `f32` equality (identical to `transforms.rs`'s
//! `scale_then_translate_applies_column_major` unit test).

// The `#[handler]` method takes `&mut self` to match the dispatch ABI
// even though this source is stateless.
#![allow(clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, OutboundReply, Resolver, actor};
use aether_kinds::Mat4Apply;
use aether_math::{Mat4, Vec4};
use aether_test_fixtures::Mat4SourceTrigger;

pub struct MatSource;

#[actor]
impl FfiActor for MatSource {
    const NAMESPACE: &'static str = "mat4_source";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(MatSource)
    }

    /// Reply the fixed `Mat4Apply` operand. A DAG `Source` always sets a
    /// reply target (the reply feeds the source's downstream handle), so
    /// the reply is unconditional; outside that context the FFI `reply`
    /// no-ops when no target is set.
    ///
    /// # Agent
    /// Driven only as a DAG `Source` node, not sent by hand — the source
    /// dispatches `aether.test_fixtures.mat4_source_trigger` and the
    /// reply (`aether.math.mat4_apply`) is the transform input.
    #[handler]
    fn on_trigger(&mut self, ctx: &mut FfiCtx<'_>, _trigger: Mat4SourceTrigger) {
        ctx.reply(&Mat4Apply {
            matrix: Mat4::from_cols_array([
                2.0, 0.0, 0.0, 0.0, //
                0.0, 3.0, 0.0, 0.0, //
                0.0, 0.0, 4.0, 0.0, //
                5.0, 6.0, 7.0, 1.0, //
            ]),
            vector: Vec4::new(1.0, 1.0, 1.0, 1.0),
        });
    }
}

aether_actor::export!(MatSource);
