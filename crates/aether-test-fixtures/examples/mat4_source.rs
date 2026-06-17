//! `mat4_source` (DAG fixtures) bundle — the `MatSource` and `Vec4Observer`
//! DAG fixtures, exported together via `export!(MatSource, Vec4Observer)`
//! (ADR-0096, issue 1994).
//!
//! # `MatSource` (entry)
//!
//! Issue 1472 DAG `Source` fixture. A computation-DAG `Source` node
//! dispatches a `Mat4SourceTrigger` to this component; the handler
//! replies a fixed, hand-computable `Mat4Apply` operand. The reply
//! resolves the source node's handle and becomes the `mat4_apply`
//! transform's input downstream (`Source → Transform`).
//!
//! # `Vec4Observer`
//!
//! Issue 1472 DAG `Observer` fixture. A computation-DAG `Observer` node
//! dispatches a `Vec4Observed` to this component with the transform's
//! resolved `Vec4` output spliced into the `input` slot. The substrate's
//! `walk_and_resolve` rewrites the resolved handle into the
//! `Ref::Inline(Vec4)` slot before dispatch, so the handler reads the
//! value directly with no guest-side handle-resolution API.
//!
//! No wire path reads a handle's *value* (`describe_handles` is
//! metadata-only), so this first-party fixture is the only way a
//! wire-driven DAG test can assert the transform actually computed
//! `M·v`. The observer surfaces the value over a wire-readable channel:
//! it writes the 16 cast bytes of the `Vec4` to `save://dag-vec4-output.bin`
//! via `aether.fs.write`, which the `FleetBench` test reads back and casts
//! to a `Vec4` for an exact `== M·v` assertion.
//!
//! Consumers load the observer from the `mat4_source` bundle with
//! `export: Some("vec4_observer")`.

// The `#[handler]` methods take `&mut self` to match the dispatch ABI
// even though these actors are stateless. `on_observed` takes `mail` by
// value to match the dispatch ABI even though it only reads the slot.
#![allow(clippy::needless_pass_by_value, clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, FfiInitCtx, MailSender, actor};
use aether_data::Ref;
use aether_kinds::{Mat4Apply, Write};
use aether_math::{Mat4, Vec4};
use aether_test_fixtures::{Mat4SourceTrigger, Vec4Observed};

/// Issue 1472 DAG `Source` fixture. Replies the fixed `Mat4Apply` operand
/// when triggered as a DAG source node.
///
/// The baked matrix is the column-major scale(2,3,4) + translate(5,6,7),
/// applied to `(1,1,1,1)` — `M·v = (7,9,11,1)`, clean integers with
/// exact `f32` equality.
pub struct MatSource;

#[actor]
impl FfiActor for MatSource {
    const NAMESPACE: &'static str = "mat4_source";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
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
    fn on_trigger(&mut self, _ctx: &mut FfiCtx<'_>, _trigger: Mat4SourceTrigger) -> Mat4Apply {
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

/// Issue 1472 DAG `Observer` fixture. Resolves the transform's `Ref<Vec4>`
/// output and surfaces the value via `aether.fs.write`.
///
/// Consumers load this actor from the `mat4_source` bundle with
/// `export: Some("vec4_observer")`.
pub struct Vec4Observer;

#[actor]
impl FfiActor for Vec4Observer {
    const NAMESPACE: &'static str = "vec4_observer";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Vec4Observer)
    }

    /// Surface the resolved `Vec4` by writing its cast bytes to a known
    /// `save` path. The DAG splices the transform output into the slot
    /// as `Ref::Inline`, so the value reads out of the `input` field;
    /// `send_to_named` needs only the `Write` kind, no capability marker.
    ///
    /// # Agent
    /// Driven only as a DAG `Observer` node — its incoming edge fills the
    /// `Ref<Vec4>` slot with the upstream transform's output.
    #[handler]
    fn on_observed(&mut self, ctx: &mut FfiCtx<'_>, mail: Vec4Observed) {
        if let Ref::Inline(v) = mail.input {
            ctx.send_to_named::<Write>(
                "aether.fs",
                &Write {
                    namespace: "save".into(),
                    path: "dag-vec4-output.bin".into(),
                    bytes: bytemuck::bytes_of(&v).to_vec(),
                },
            );
        }
    }
}

aether_actor::export!(MatSource, Vec4Observer);
