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

// The `#[handler]` method takes `&mut self` and `mail` by value to match
// the dispatch ABI even though this observer is stateless and only reads
// the mail's slot.
#![allow(clippy::needless_pass_by_value, clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, MailSender, Resolver, actor};
use aether_data::Ref;
use aether_kinds::Write;
use aether_test_fixtures::Vec4Observed;

pub struct Vec4Observer;

#[actor]
impl FfiActor for Vec4Observer {
    const NAMESPACE: &'static str = "vec4_observer";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
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

aether_actor::export!(Vec4Observer);
