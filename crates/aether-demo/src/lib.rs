//! `aether-demo` — the release demo's bring-up component.
//!
//! The demo is four components in a boot list: the kit camera `main`, the kit
//! camera controller, the kit mesh viewer, and [`Demo`], loaded last. The
//! viewer draws nothing until something sends it `aether.kit.mesh.load`, the
//! renderer holds its identity view until something tells it whose view to
//! take, and a boot list sends no mail, so [`Demo`] is that something: at
//! `wire` it sends the viewer the same load an operator would and sends the
//! renderer `aether.render.view_from` naming the camera, and logs each reply.
//! It adds no second way to do either; bring-up is ordinary mail from an
//! ordinary component, which is why it lives in its own throwaway crate
//! rather than in the kit or the engine.
//!
//! [`Demo`] declares the viewer and the renderer as dependencies (ADR-0230),
//! so the component host refuses to create it unless both are live. The
//! subject is two module consts, not config: the demo exists to draw one
//! picture, and changing it is a one-line edit.

#![forbid(unsafe_code)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::MeshLoadResult;
use aether_kit::camera::CameraComponent;
use aether_kit::mesh::{LoadMesh, MeshViewer};
use aether_render::{RenderCapability, ViewFrom, ViewFromResult, ViewSource};

/// The `aether.fs` namespace the subject is read from: the chassis's asset
/// root (`--assets-dir`, or a depot's `pack/assets`).
const SUBJECT_NAMESPACE: &str = "assets";
/// The subject, relative to [`SUBJECT_NAMESPACE`]. Any `.dsl` or `.obj` under
/// the asset root works; `box.dsl` and `utah_teapot.obj` sit beside this one.
const SUBJECT_PATH: &str = "teapot.dsl";

/// Sends the mesh viewer its one load and the renderer its camera at `wire`,
/// and logs how each went.
pub struct Demo {
    subject: LoadMesh,
}

#[actor(root, depends(MeshViewer, RenderCapability))]
impl WasmActor for Demo {
    const NAMESPACE: &'static str = "aether.demo";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { subject: LoadMesh { namespace: SUBJECT_NAMESPACE.to_owned(), path: SUBJECT_PATH.to_owned() } })
    }

    /// Ask the viewer to load the subject, and the renderer to take its view
    /// from the camera `main`. Each replies to this send's sender, so the
    /// results arrive at [`Self::on_mesh_load_result`] and
    /// [`Self::on_view_from_result`].
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.send::<MeshViewer>(&self.subject);
        ctx.send::<RenderCapability>(&ViewFrom { source: CameraComponent::main_path().narrow::<ViewSource>() });
        Ok(())
    }

    /// The viewer's answer to the load: parsed and drawing, or why not.
    #[handler::response]
    fn on_mesh_load_result(&mut self, _ctx: &mut WasmCtx<'_>, result: MeshLoadResult) {
        match result.error {
            None => tracing::info!(path = %self.subject.path, "subject loaded"),
            Some(error) => tracing::error!(path = %self.subject.path, %error, "subject failed to load"),
        }
    }

    /// The renderer's answer to `view_from`: following the camera, or why
    /// its path did not prove.
    #[handler::response]
    fn on_view_from_result(&mut self, _ctx: &mut WasmCtx<'_>, result: ViewFromResult) {
        let _ = self;
        match result {
            ViewFromResult::Ok => tracing::info!("the renderer follows the camera"),
            ViewFromResult::Err(refused) => {
                tracing::error!(path = %refused.path, reason = ?refused.reason, "the renderer does not follow the camera");
            }
        }
    }
}

aether_actor::export!(public = [Demo]);
