//! `mat4_source` fixture bundle — the `MatSource` fixture.
//!
//! # `MatSource` (entry)
//!
//! Issue 1472 `Source` fixture. The handler replies a fixed,
//! hand-computable `Mat4Apply` operand when triggered with a
//! `Mat4SourceTrigger`.

// The `#[handler]` method takes `&mut self` to match the dispatch ABI
// even though the actor is stateless.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Mat4Apply;
use aether_math::{Mat4, Vec4};
use aether_test_fixtures_kinds::Mat4SourceTrigger;

/// Issue 1472 `Source` fixture. Replies the fixed `Mat4Apply` operand
/// when triggered.
///
/// The baked matrix is the column-major scale(2,3,4) + translate(5,6,7),
/// applied to `(1,1,1,1)` — `M·v = (7,9,11,1)`, clean integers with
/// exact `f32` equality.
pub struct MatSource;

#[actor]
impl WasmActor for MatSource {
    const NAMESPACE: &'static str = "mat4_source";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(MatSource)
    }

    /// Reply the fixed `Mat4Apply` operand when the source is triggered.
    ///
    /// # Agent
    /// Dispatch `aether.test_fixtures.mat4_source_trigger`; the reply
    /// (`aether.math.mat4_apply`) is the operand.
    #[handler]
    fn on_trigger(&mut self, _ctx: &mut WasmCtx<'_>, _trigger: Mat4SourceTrigger) -> Mat4Apply {
        Mat4Apply {
            matrix: Mat4::from_cols_array([
                2.0, 0.0, 0.0, 0.0, //
                0.0, 3.0, 0.0, 0.0, //
                0.0, 0.0, 4.0, 0.0, //
                5.0, 6.0, 7.0, 1.0, //
            ]),
            vector: Vec4::new(1.0, 1.0, 1.0, 1.0),
        }
    }
}
