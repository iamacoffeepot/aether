// Camera and stroke maths: bounded counts cast to f32 are domain-correct.
#![allow(clippy::cast_precision_loss)]
// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the trampoline owns the payload and hands it off.
#![allow(clippy::needless_pass_by_value)]

//! The mascot, drawn as pen-plotter line art on a live substrate.
//!
//! It never asks what colour a pixel is. It asks which lines an illustrator
//! would draw on the surface, solves them against the real geometry,
//! decides which survive, and emits them as stroke ribbons.
//!
//! ```text
//! extract  ->  visibility  ->  weld  ->  ribbon  ->  aether.render
//! ```
//!
//! Everything drawn is a **level set of a per-vertex scalar**, which is why
//! one piece of machinery produces every feature kind: the silhouette is
//! the zero set of `view . normal`, hatching is the level sets of
//! `position . axis`, and creases are the level sets of surface relief —
//! a band-pass of the mesh against itself, projected on the normal.
//!
//! # What is cached and what is not
//!
//! Hatch and crease are properties of the surface, not of the viewer, so
//! they are extracted once at load and kept. Only the silhouette and the
//! visibility split are per-frame, because only those two depend on where
//! the eye is. The offline renderer recomputes everything every frame
//! because it has no reason not to; here that difference is most of the
//! frame budget.
//!
//! # Lifecycle
//!
//! 1. `aether.puppet.load { namespace, path }` points at an `.obj` inside
//!    one of the substrate's I/O namespaces.
//! 2. The component fires `aether.fs.read` and waits.
//! 3. On reply the mesh is parsed, the view-independent passes run, and
//!    the cache is replaced atomically. A failed load leaves the previous
//!    subject on screen rather than blanking it.
//! 4. Every `aether.lifecycle.render` stage publishes the camera and
//!    re-emits the drawing.

pub mod extract;
pub mod feature;
mod kinds;
pub mod labels;
pub mod math3;
pub mod mesh;
pub mod ribbon;
pub mod style;
pub mod visibility;
pub mod weld;

pub use kinds::*;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_fs::{FsCapability, FsMailboxExt, ReadResult};
use aether_kinds::Render;
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::{Mat4, Vec3};
use aether_render::{DrawTriangle, RenderCapability, ViewProjection};
use serde::{Deserialize, Serialize};

use feature::Curve3;
use mesh::Mesh;

/// Vertical field of view, in radians. Fixed rather than configurable:
/// the framing knob people reach for is distance, and two ways to make the
/// subject bigger is one too many.
const FIELD_OF_VIEW: f32 = 0.454;

/// Assumed aspect until a window size arrives. Portrait, because she is.
const ASPECT: f32 = 0.78;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.load_context")]
struct LoadContext {
    namespace: String,
    path: String,
}

pub struct Puppet {
    subject: Option<Mesh>,
    /// The view-independent drawing: hatch and crease, welded, kept from
    /// load. Rebuilt only when the subject changes.
    surface: Vec<Curve3>,
    settings: extract::Settings,
    look: Look,
}

impl Puppet {
    fn eye(&self) -> Vec3 {
        let (azimuth, elevation) = (self.look.azimuth.to_radians(), self.look.elevation.to_radians());
        let (sin_a, cos_a) = azimuth.sin_cos();
        let (sin_e, cos_e) = elevation.sin_cos();

        self.target() + Vec3::new(sin_a * cos_e, sin_e, cos_a * cos_e) * self.look.distance
    }

    fn target(&self) -> Vec3 {
        Vec3::new(0.0, self.look.height, 0.0)
    }

    fn view_projection(&self) -> ViewProjection {
        let view = Mat4::look_at_rh(self.eye(), self.target(), Vec3::new(0.0, 1.0, 0.0));
        let projection = Mat4::perspective_rh(FIELD_OF_VIEW, ASPECT, 0.05, 40.0);

        ViewProjection { view_proj: (projection * view).to_cols_array() }
    }
}

#[actor]
impl WasmActor for Puppet {
    const NAMESPACE: &'static str = "aether.puppet";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            subject: None,
            surface: Vec::new(),
            settings: extract::Settings::default(),
            // Facing her, slightly above, far enough back that her whole
            // height fits the frame at this field of view.
            look: Look { azimuth: 0.0, elevation: 3.0, distance: 3.2, height: 0.0 },
        })
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Render>();
    }

    /// Point her at a subject. Asynchronous — the reply target is carried
    /// in the fs context so the eventual `LoadResult` reaches whoever asked.
    #[handler::single]
    fn on_load(&mut self, ctx: &mut WasmCtx<'_>, mail: Load) {
        let context = LoadContext { namespace: mail.namespace, path: mail.path };
        ctx.actor::<FsCapability>().with_context(&context).read(&context.namespace, &context.path);
    }

    /// The bytes arrived. Parse, run the view-independent passes, and swap
    /// the cache in one go.
    #[handler::single]
    fn on_read(&mut self, _ctx: &mut WasmCtx<'_>, mail: ReadResult) {
        let ReadResult::Ok { bytes, .. } = mail else {
            tracing::warn!(target: "aether_puppet", "read failed; keeping the previous subject");
            return;
        };

        let Some(subject) = Mesh::from_obj_bytes(&bytes, self.settings.relaxation) else {
            tracing::warn!(target: "aether_puppet", "parse failed; keeping the previous subject");
            return;
        };

        // Hatch and crease describe the surface, not the view, so they are
        // solved once here rather than every frame.
        self.surface = extract::surface(&subject, None, &self.settings);
        tracing::info!(
            target: "aether_puppet",
            faces = subject.faces.len(),
            curves = self.surface.len(),
            "subject loaded",
        );
        self.subject = Some(subject);
    }

    #[handler::single]
    fn on_look(&mut self, _ctx: &mut WasmCtx<'_>, mail: Look) {
        self.look = mail;
    }

    /// One frame: publish the camera, add the view-dependent lines to the
    /// cached ones, split them against the surface, and emit ribbons.
    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _stage: Render) {
        let render = ctx.actor::<RenderCapability>();
        render.send(&self.view_projection());

        let Some(subject) = self.subject.as_ref() else {
            return;
        };

        let eye = self.eye();
        let mut triangles: Vec<DrawTriangle> = Vec::new();
        let drawing = self.surface.iter().cloned().chain(extract::silhouettes(subject, eye));

        for curve in drawing {
            for run in visibility::runs(subject, eye, &curve, &|_| true, visibility::Mode::Opaque) {
                ribbon::ribbon(&run, eye, 0, &mut triangles);
            }
        }

        if !triangles.is_empty() {
            render.send_many(&triangles);
        }
    }
}

aether_actor::export!(Puppet);
