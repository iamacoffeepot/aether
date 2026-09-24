//! `aether-demo` — the release demo's bring-up component.
//!
//! The demo is four components in a boot list: the kit camera, the kit camera
//! controller, the kit mesh viewer, and [`Demo`], loaded last. The viewer draws
//! nothing until something sends it `aether.kit.mesh.load`, and a boot list
//! sends no mail, so [`Demo`] is that something: at `wire` it sends the viewer
//! the same load an operator would, and logs the viewer's reply. It adds no
//! second way to load a mesh; bring-up is ordinary mail from an ordinary
//! component, which is why it lives in its own throwaway crate rather than in
//! the kit or the engine.
//!
//! [`Demo`] declares the viewer as a dependency (ADR-0230), so the component
//! host refuses to create it unless the viewer is already live under its
//! default name. The subject is two module consts, not config: the demo exists
//! to draw one picture, and changing it is a one-line edit.

#![forbid(unsafe_code)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::MeshLoadResult;
use aether_kit::mesh::{LoadMesh, MeshViewer};

/// The `aether.fs` namespace the subject is read from: the chassis's asset
/// root (`--assets-dir`, or a depot's `pack/assets`).
const SUBJECT_NAMESPACE: &str = "assets";
/// The subject, relative to [`SUBJECT_NAMESPACE`]. Any `.dsl` or `.obj` under
/// the asset root works; `box.dsl` and `utah_teapot.obj` sit beside this one.
const SUBJECT_PATH: &str = "teapot.dsl";

/// Sends the mesh viewer its one load at `wire` and logs how it went.
pub struct Demo {
    subject: LoadMesh,
}

#[actor(depends(MeshViewer))]
impl WasmActor for Demo {
    const NAMESPACE: &'static str = "aether.demo";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { subject: LoadMesh { namespace: SUBJECT_NAMESPACE.to_owned(), path: SUBJECT_PATH.to_owned() } })
    }

    /// Ask the viewer to load the subject. The viewer replies to this send's
    /// sender, so the result arrives at [`Self::on_mesh_load_result`].
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.send::<MeshViewer>(&self.subject);
    }

    /// The viewer's answer to the load: parsed and drawing, or why not.
    #[handler::single]
    fn on_mesh_load_result(&mut self, _ctx: &mut WasmCtx<'_>, result: MeshLoadResult) {
        match result.error {
            None => tracing::info!(path = %self.subject.path, "subject loaded"),
            Some(error) => tracing::error!(path = %self.subject.path, %error, "subject failed to load"),
        }
    }
}

aether_actor::export!(default = Demo);
