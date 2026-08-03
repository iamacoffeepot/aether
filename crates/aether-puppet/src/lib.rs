// Geometry throughout, and the numeric lints are answered the way
// `aether-math` and `aether-mesh` answer them. Counts and indices cast to
// and from `f32` are bounded by the mesh; `mul_add` changes float
// semantics for no gain the eye can see; and `a`/`b`/`c` for the corners
// of a triangle is the clearest name those have.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::many_single_char_names
)]
// The renderer is a pipeline of pure transforms, so very nearly every
// function returns something. Marking each one adds noise without
// catching anything a caller could plausibly get wrong.
#![allow(clippy::must_use_candidate, clippy::return_self_not_must_use)]
// Handlers take `&mut self` because the ADR-0033 dispatch ABI says so, not
// because each one needs it: a handler that only forwards to a capability
// touches no state at all.
#![allow(clippy::unused_self)]
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

use aether_actor::{ActorInitError, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_fs::{FsCapability, FsMailboxExt, ReadResult};
use aether_kinds::{MouseButton, MouseButtonRelease, MouseMove, MouseWheel, Render, WindowSize};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::{Mat4, Vec2, Vec3};
use aether_render::{DrawTriangle, RenderCapability, ViewProjection};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};
use serde::{Deserialize, Serialize};

use feature::Curve3;
use mesh::Mesh;

/// Vertical field of view, in radians. Fixed rather than configurable:
/// the framing knob people reach for is distance, and two ways to make the
/// subject bigger is one too many.
const FIELD_OF_VIEW: f32 = 0.454;

/// Aspect assumed only until the first `WindowSize` arrives.
///
/// It cannot stay a constant: the projection's horizontal scale divides by
/// it, so a guess that disagrees with the surface stretches the drawing on
/// one axis and keeps stretching it as the window is resized.
const ASPECT_UNTIL_MEASURED: f32 = 4.0 / 3.0;

/// Padding the material field was baked with, as a fraction of the mesh's
/// longest axis. The lattice is reconstructed from the mesh bounds by the
/// same rule, so no transform rides alongside the volume.
const LABEL_PAD: f32 = 0.12;

/// Drag sensitivity. A full sweep of a 900-pixel window turns her a bit
/// more than half a revolution, which is about where a drag stops feeling
/// like shoving and starts feeling like turning.
const ORBIT_DEGREES_PER_PIXEL: f32 = 0.25;

/// Fraction of the current distance one wheel notch covers.
const DOLLY_PER_NOTCH: f32 = 0.08;

/// Cast an occlusion ray every Nth point rather than at all of them.
///
/// A curve's points sit a triangle apart, far finer than the scale at which
/// occlusion actually changes — an edge a stroke disappears behind is many
/// points wide. Sampling every third and holding the verdict between
/// samples costs a point or two of precision at an occluding edge and takes
/// two thirds of the rays off the frame.
const VISIBILITY_STRIDE: usize = 3;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.load_context")]
struct LoadContext {
    /// Only the mesh read carries one. The field read is a dependency of
    /// the same request, not a request of its own, so it must not answer.
    reply: Option<ReplyHandle>,
    namespace: String,
    path: String,
}

pub struct Puppet {
    subject: Option<Mesh>,
    /// The subject again at a tenth of the faces, and the only thing the
    /// silhouette is solved against.
    ///
    /// Marching 868k faces every time the eye moves is the whole of the
    /// per-frame geometry cost once hatch and crease are cached at load,
    /// and a silhouette does not need them: it is where the surface turns
    /// away from the viewer, which is a property of the form rather than of
    /// the carving. Creases and occlusion keep the fine mesh, where the
    /// detail is what is being asked about.
    silhouette_subject: Option<Mesh>,
    /// The material field, and the path still owed for it. Both assets
    /// arrive as separate mail, in whichever order the reads settle, so
    /// extraction waits until nothing is outstanding rather than running
    /// twice.
    labels: Option<labels::Labels>,
    awaiting_labels: Option<String>,
    /// Where the cursor was on the last move, and whether a button is
    /// down. Only the delta between two moves means anything for an orbit,
    /// and the substrate reports absolute window positions, so the previous
    /// one has to be kept here.
    dragging: bool,
    cursor: Vec2,
    aspect: f32,
    /// Last frame's triangles, and the eye they were solved for.
    ///
    /// Keyed on the eye alone, which is sound only while the mesh is
    /// static: once a pose exists the eye can be unchanged while the
    /// drawing is not, and this would serve a stale frame. The key has to
    /// carry the pose too — iamacoffeepot/aether#4336.
    ///
    /// Silhouette and visibility are the only view-dependent work, so a
    /// frame drawn from an eye we have already solved is the same frame.
    /// The window redraws continuously whether or not anything moved, and
    /// without this every one of those redraws re-marched 868k faces and
    /// re-cast every occlusion ray to arrive at the identical answer.
    drawn: Vec<DrawTriangle>,
    drawn_from: Option<Vec3>,
    /// Held until extraction runs, because the answer is not known until
    /// both assets have landed.
    owed: Option<ReplyHandle>,
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
        let projection = Mat4::perspective_rh(FIELD_OF_VIEW, self.aspect, 0.05, 40.0);

        ViewProjection { view_proj: (projection * view).to_cols_array() }
    }
}

#[actor]
impl WasmActor for Puppet {
    const NAMESPACE: &'static str = "aether.puppet";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            subject: None,
            silhouette_subject: None,
            labels: None,
            awaiting_labels: None,
            dragging: false,
            cursor: Vec2::new(0.0, 0.0),
            aspect: ASPECT_UNTIL_MEASURED,
            drawn: Vec::new(),
            drawn_from: None,
            owed: None,
            surface: Vec::new(),
            settings: extract::Settings::default(),
            // Facing her, slightly above, far enough back that her whole
            // height fits the frame at this field of view.
            look: Look { azimuth: 0.0, elevation: 3.0, distance: 5.4, height: 0.0 },
        })
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Render>();

        // Window-originated input is addressed through the window identity
        // (ADR-0164), not the lifecycle stream — subscribing to these on
        // lifecycle compiles and silently delivers nothing, which is how
        // the first attempt at a drag camera did exactly nothing.
        let window = ctx.actor::<WindowCapability>();
        window.subscribe::<MouseButton>(WindowSelector::All);
        window.subscribe::<MouseButtonRelease>(WindowSelector::All);
        window.subscribe::<MouseMove>(WindowSelector::All);
        window.subscribe::<MouseWheel>(WindowSelector::All);
        window.subscribe::<WindowSize>(WindowSelector::All);
    }

    /// The surface changed shape, so the projection has to follow it.
    #[handler::single]
    fn on_window_size(&mut self, _ctx: &mut WasmCtx<'_>, size: WindowSize) {
        if size.width > 0 && size.height > 0 {
            self.aspect = size.width as f32 / size.height as f32;
        }
    }

    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        self.dragging = true;
        self.cursor = Vec2::new(press.x, press.y);
    }

    #[handler::single]
    fn on_mouse_release(&mut self, _ctx: &mut WasmCtx<'_>, _release: MouseButtonRelease) {
        self.dragging = false;
    }

    /// Drag to orbit. Horizontal sweeps her around, vertical raises and
    /// lowers the eye — clamped short of the poles, where an orbit camera's
    /// up vector degenerates and the view rolls.
    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        let at = Vec2::new(moved.x, moved.y);
        let delta = at - self.cursor;
        self.cursor = at;

        if self.dragging {
            self.look.azimuth -= delta.x * ORBIT_DEGREES_PER_PIXEL;
            self.look.elevation = (self.look.elevation + delta.y * ORBIT_DEGREES_PER_PIXEL).clamp(-85.0, 85.0);
        }
    }

    /// Wheel to dolly. Proportional rather than additive, so a step feels
    /// the same close up as far out.
    #[handler::single]
    fn on_mouse_wheel(&mut self, _ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        self.look.distance = (self.look.distance * (1.0 - wheel.delta_y * DOLLY_PER_NOTCH)).clamp(0.6, 40.0);
    }

    /// Point her at a subject. Asynchronous — the reply target is carried
    /// in the fs context so the eventual `LoadResult` reaches whoever asked.
    #[handler::manual]
    fn on_load(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: Load) {
        self.owed = ctx.reply_target();

        if !mail.labels.is_empty() {
            self.awaiting_labels = Some(mail.labels.clone());
            let context = LoadContext { reply: None, namespace: mail.namespace.clone(), path: mail.labels.clone() };
            ctx.actor::<FsCapability>().with_context(&context).read(&mail.namespace, &mail.labels);
        }

        let context = LoadContext { reply: None, namespace: mail.namespace, path: mail.path };
        ctx.actor::<FsCapability>().with_context(&context).read(&context.namespace, &context.path);
    }

    /// Answer the load, once, with whatever actually happened.
    ///
    /// A loader that cannot report failure is a bad surface and this one
    /// proved it: a mesh that overran the mail bound reported `delivered`
    /// to the caller and left the reason only in the actor log.
    fn settle(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: &LoadResult) {
        if let Some(sender) = self.owed.take() {
            ctx.reply_to(sender, result);
        }
    }

    /// The bytes arrived. Parse, run the view-independent passes, and swap
    /// the cache in one go.
    #[handler::manual]
    fn on_read(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: ReadResult) {
        let path = match mail {
            ReadResult::Ok { ref path, .. } => path.clone(),
            ReadResult::Err { ref path, ref error, .. } => {
                tracing::warn!(target: "aether_puppet", path = %path, error = ?error, "read failed");
                let reason = format!("read {path} failed: {error:?}");
                self.settle(ctx, &LoadResult::Err { reason });
                return;
            }
        };
        let ReadResult::Ok { bytes, .. } = mail else {
            return;
        };

        // Which asset this is, decided by the path the reply echoes rather
        // than by arrival order — the two reads settle independently.
        if self.awaiting_labels.as_deref() == Some(path.as_str()) {
            self.awaiting_labels = None;
            let bounds = self.subject.as_ref().map_or((Vec3::splat(-1.0), Vec3::splat(1.0)), |m| (m.min, m.max));
            self.labels = labels::Labels::parse(&bytes, bounds.0, bounds.1, LABEL_PAD);
            if self.labels.is_none() {
                tracing::warn!(target: "aether_puppet", "material field is not a cube; creases stay unmasked");
            }
        } else {
            let Some(subject) = Mesh::from_obj_bytes(&bytes, self.settings.relaxation) else {
                tracing::warn!(target: "aether_puppet", "parse failed; keeping the previous subject");
                self.settle(ctx, &LoadResult::Err { reason: format!("{path} is not a mesh this reader accepts") });
                return;
            };
            // A lattice too coarse for the subject's own feature scale
            // leaves nothing to draw on, so the fine mesh stands in rather
            // than the outline disappearing.
            self.silhouette_subject = (self.settings.silhouette_cells > 0)
                .then(|| subject.coarsened(self.settings.silhouette_cells, self.settings.relaxation))
                .flatten();
            self.subject = Some(subject);
        }

        // Extraction needs the mesh, and the field if one was asked for —
        // the lattice is placed against the mesh's own bounds, so a field
        // that lands first has to be re-placed once the mesh arrives.
        let Some(subject) = self.subject.as_ref() else {
            return;
        };
        if self.awaiting_labels.is_some() {
            return;
        }

        // Hatch and crease describe the surface, not the view, so they are
        // solved once here rather than every frame.
        self.surface = extract::surface(subject, self.labels.as_ref(), &self.settings);
        self.drawn_from = None;
        tracing::info!(
            target: "aether_puppet",
            faces = subject.faces.len(),
            curves = self.surface.len(),
            masked = self.labels.is_some(),
            "subject loaded",
        );

        let settled = LoadResult::Ok { vertices: subject.positions.len() as u32, faces: subject.faces.len() as u32 };
        self.settle(ctx, &settled);
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
        if self.drawn_from == Some(eye) {
            render.send_many(&self.drawn);
            return;
        }

        // The silhouette is solved on the coarse mesh and the cached
        // surface curves on the fine one, so each carries its own mesh's
        // ray bias into the visibility split — a coarse point tested at the
        // fine mesh's bias sits inside its own subject and the outline
        // comes back dashed.
        let silhouette_mesh = self.silhouette_subject.as_ref().unwrap_or(subject);
        let mut triangles: Vec<DrawTriangle> = Vec::new();
        let drawing = self.surface.iter().cloned().map(|curve| (curve, subject.surface_bias())).chain(
            extract::silhouettes(silhouette_mesh, eye).into_iter().map(|curve| (curve, silhouette_mesh.surface_bias())),
        );

        for (curve, bias) in drawing {
            // Tone gating already happened at load — it does not depend on
            // the eye — so all that is left per frame is occlusion, and
            // that is always asked of the fine mesh: what stands in front
            // of a stroke is a question about the real surface.
            for run in
                visibility::runs(subject, eye, &curve, &|_| true, visibility::Mode::Opaque, VISIBILITY_STRIDE, bias)
            {
                ribbon::ribbon(&run, eye, 0, &mut triangles);
            }
        }

        if !triangles.is_empty() {
            render.send_many(&triangles);
        }
        self.drawn = triangles;
        self.drawn_from = Some(eye);
    }
}

aether_actor::export!(Puppet);
