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
//! Nearly everything drawn is a **level set of a per-vertex scalar**, which
//! is why one piece of machinery produces most feature kinds: the silhouette
//! is the zero set of `view . normal`, hatching is the level sets of
//! `position . axis`, and creases are the level sets of surface relief — a
//! band-pass of the mesh against itself, projected on the normal.
//!
//! The face is the exception, and it has to be. Her eyes carry no relief at
//! all, because a pupil is painted texture over a smooth ball, and her lips
//! clear the crease threshold about a tenth of the time. So the eye, brow,
//! mouth and nose are **authored**: `chart` says what each looks like,
//! `anchor` says where it goes — measured off the material field, never
//! guessed — and `plant` drops the marks onto a plane fitted through the
//! surface beneath them.
//!
//! # What is cached and what is not
//!
//! Hatch and crease are properties of the surface, not of the viewer, so
//! they are extracted once at load and kept, and so are the anchors the
//! chart draws around. Per frame there is the silhouette, the charted face
//! (its nose bar retires once she turns far enough for the profile to draw
//! her nose itself), the suggestive contours, and the visibility split —
//! everything that depends on where the eye is, and nothing else. The
//! offline renderer recomputes all of it every frame because it has no
//! reason not to; here that difference is most of the frame budget.
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

pub mod anchor;
pub mod chart;
pub mod easel;
pub mod extract;
pub mod feature;
mod kinds;
pub mod labels;
pub mod math3;
pub mod mesh;
pub mod plant;
pub mod ribbon;
pub mod strokes;
pub mod style;
pub mod visibility;
pub mod weld;

pub use kinds::*;

use aether_actor::{ActorInitError, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_fs::{FsCapability, FsMailboxExt, ReadResult};
use aether_kinds::{MouseButton, MouseButtonRelease, MouseMove, MouseWheel, Render, WindowSize};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::{Mat4, Vec2, Vec3};
use aether_render::{
    CreateGeometryResult, CreateTextureResult, DrawTriangle, ProgramRegisterResult, RenderCapability, ViewProjection,
};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

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
/// points wide. Sampling every third takes two thirds of the rays off the
/// frame, and the split refines any window whose ends disagree, so a stroke
/// still ends on the edge it disappears behind rather than at the next
/// sample.
///
/// Measured on the kitsune at the default framing, with the silhouette
/// solved on the fine mesh: casting at every point instead costs 15.5ms of
/// the eye-moved path against 9.6ms sampled-and-refined, which is most of
/// a frame's headroom for a drawing that differs by a hundredth of a
/// percent of its points.
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

/// Diagnostic tap on the wash's inputs: bake the planes a develop at the
/// current view would paint from and write them raw — `dims.txt`
/// (`width height`), `label.bin` (u8 per pixel), `tone.bin` /
/// `facing.bin` (little-endian f32 per pixel) — under `prefix` in
/// `namespace`, for offline diff against the reference board's baked map
/// and for cross-feeding the CPU wash oracle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.dump_planes")]
pub struct DumpPlanes {
    /// Writable `aether.fs` namespace to dump into, e.g. `save`.
    namespace: String,
    /// Directory prefix the four plane files are written under.
    prefix: String,
}

pub struct Puppet {
    subject: Option<Mesh>,
    /// The material field, and the path still owed for it. Both assets
    /// arrive as separate mail, in whichever order the reads settle, so
    /// extraction waits until nothing is outstanding rather than running
    /// twice.
    labels: Option<labels::Labels>,
    /// The subject's per-vertex blurred-indicator scores
    /// ([`labels::Labels::vertex_scores`]), the wash's classification
    /// signal. Solved once when mesh and field are both in.
    class_scores: Option<Vec<[f32; labels::CLASSES]>>,
    awaiting_labels: Option<String>,
    /// Where the charted face goes on this subject, measured off the field
    /// when the subject changes. Neither the mesh nor the field moves, so
    /// neither does the answer, and the eye scan walks every vertex.
    anchors: Option<anchor::Anchors>,
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
    curves: Vec<Curve3>,
    drawn_from: Option<Vec3>,
    /// Held until extraction runs, because the answer is not known until
    /// both assets have landed.
    owed: Option<ReplyHandle>,
    /// The view-independent drawing: hatch and crease, welded, kept from
    /// load. Rebuilt only when the subject changes.
    surface: Vec<Curve3>,
    settings: extract::Settings,
    look: Look,
    /// The wash layer under the ink (#4349): a painted sheet standing
    /// behind the subject, re-developed when the view settles.
    easel: easel::Easel,
    /// The ink layer over it (ADR-0172): the visibility field and the
    /// stroke program, re-solved whenever the eye moves.
    strokes: strokes::Strokes,
    /// Which layer each in-flight reply belongs to. The easel and the
    /// ink both register programs and create textures and geometry, and
    /// the replies carry no sender — so the answer is position: the
    /// render cap replies to one kind in the order that kind was sent.
    awaiting: Awaiting3,
}

/// The layer an in-flight ask belongs to.
#[derive(Clone, Copy)]
enum Awaiting {
    Easel,
    Strokes,
}

/// One queue per reply kind, since the three kinds interleave freely.
#[derive(Default)]
struct Awaiting3 {
    registers: VecDeque<Awaiting>,
    textures: VecDeque<Awaiting>,
    geometries: VecDeque<Awaiting>,
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

    fn view_matrix(&self) -> Mat4 {
        let view = Mat4::look_at_rh(self.eye(), self.target(), Vec3::new(0.0, 1.0, 0.0));
        let projection = Mat4::perspective_rh(FIELD_OF_VIEW, self.aspect, 0.05, 40.0);

        projection * view
    }

    fn view_projection(&self) -> ViewProjection {
        ViewProjection { view_proj: self.view_matrix().to_cols_array() }
    }

    /// The charted face, planted on `subject`. Empty without a material
    /// field, since every anchor is measured from one — a face drawn on a
    /// guess is worse than no face.
    fn face(&self, subject: &Mesh, eye: Vec3) -> Vec<Curve3> {
        let (Some(anchors), Some(face)) = (self.anchors.as_ref(), self.settings.face) else {
            return Vec::new();
        };

        chart::marks(subject, anchors, face, &self.settings, eye)
    }
}

#[actor]
impl WasmActor for Puppet {
    const NAMESPACE: &'static str = "aether.puppet";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            subject: None,
            labels: None,
            class_scores: None,
            awaiting_labels: None,
            anchors: None,
            dragging: false,
            cursor: Vec2::new(0.0, 0.0),
            aspect: ASPECT_UNTIL_MEASURED,
            curves: Vec::new(),
            drawn_from: None,
            owed: None,
            surface: Vec::new(),
            settings: extract::Settings::default(),
            easel: easel::Easel::default(),
            strokes: strokes::Strokes::default(),
            awaiting: Awaiting3::default(),
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

    /// The surface changed shape, so the projection has to follow it —
    /// and the easel's canvas with it.
    #[handler::single]
    fn on_window_size(&mut self, _ctx: &mut WasmCtx<'_>, size: WindowSize) {
        if size.width > 0 && size.height > 0 {
            self.aspect = size.width as f32 / size.height as f32;
            self.easel.resized(size.width, size.height);
            self.strokes.resized(size.width, size.height);
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

        // The field's lattice is placed against the mesh's own bounds, and
        // a field that settled before its mesh was placed against stand-in
        // bounds — re-place it now that both are in (issue 4401).
        if let Some(labels) = self.labels.as_mut() {
            labels.place_against(subject.min, subject.max, LABEL_PAD);
        }

        // Where her features are, measured off the field before anything is
        // drawn on them. Placement is derived; only the shape is authored.
        self.anchors = self.labels.as_ref().and_then(|labels| anchor::Anchors::measure(subject, labels));
        self.class_scores = self.labels.as_ref().map(|labels| labels.vertex_scores(&subject.positions));

        // Hatch and crease describe the surface, not the view, so they are
        // solved once here rather than every frame.
        self.surface = extract::surface(subject, self.labels.as_ref(), self.anchors.as_ref(), &self.settings);
        self.drawn_from = None;
        self.easel.subject_changed();
        self.strokes.subject_changed(subject);
        tracing::info!(
            target: "aether_puppet",
            faces = subject.faces.len(),
            curves = self.surface.len(),
            masked = self.labels.is_some(),
            eyes = self.anchors.as_ref().map_or(0, |at| at.eyes.len()),
            "subject loaded",
        );

        let settled = LoadResult::Ok { vertices: subject.positions.len() as u32, faces: subject.faces.len() as u32 };
        self.settle(ctx, &settled);
    }

    #[handler::single]
    fn on_look(&mut self, _ctx: &mut WasmCtx<'_>, mail: Look) {
        self.look = mail;
    }

    /// Bake and dump the easel's planes at the current view — the
    /// diagnostic [`DumpPlanes`] asks for. Fire-and-forget: the planes
    /// land as files, the harness reads them off the disk.
    #[handler::single]
    fn on_dump_planes(&mut self, ctx: &mut WasmCtx<'_>, dump: DumpPlanes) {
        let Some((subject, scores)) = self.subject.as_ref().zip(self.class_scores.as_ref()) else {
            tracing::warn!(target: "aether_puppet", "dump_planes: no subject loaded");
            return;
        };
        let view = easel::View {
            eye: self.eye(),
            target: self.target(),
            view_proj: self.view_matrix(),
            aspect: self.aspect,
            field_of_view: FIELD_OF_VIEW,
        };
        let (curves, eye) = (&self.curves, self.eye());
        let split = || Self::split(subject, curves, eye);
        let painted = easel::Subject { mesh: subject, scores, settings: &self.settings, drawn: &split, chart: None };
        let Some(planes) = self.easel.bake_planes(&painted, &view) else {
            tracing::warn!(target: "aether_puppet", "dump_planes: no canvas before the first resize");
            return;
        };

        let fs = ctx.actor::<FsCapability>();
        fs.write(&dump.namespace, format!("{}/dims.txt", dump.prefix), format!("{} {}", planes.width, planes.height));
        fs.write(&dump.namespace, format!("{}/label.bin", dump.prefix), planes.class.clone());
        for (name, plane) in [("tone", &planes.tone), ("facing", &planes.facing)] {
            let bytes: Vec<u8> = plane.iter().flat_map(|at| at.to_le_bytes()).collect();
            fs.write(&dump.namespace, format!("{}/{name}.bin", dump.prefix), bytes);
        }
        tracing::info!(
            target: "aether_puppet",
            width = planes.width,
            height = planes.height,
            prefix = %dump.prefix,
            "dump_planes written",
        );
    }

    /// One frame: publish the camera, add the view-dependent lines to the
    /// cached ones, split them against the surface, and emit ribbons over
    /// the easel's sheet.
    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _stage: Render) {
        let render = ctx.actor::<RenderCapability>();
        render.send(&self.view_projection());

        let Some(subject) = self.subject.as_ref() else {
            return;
        };

        let eye = self.eye();
        if self.drawn_from != Some(eye) {
            // The face rides the per-eye path rather than the cached
            // surface, because the nose bar retires once her face turns
            // over its own bridge and that is a question about where the
            // eye is. Suggestive contours are there for the same reason —
            // they are the silhouette one derivative out.
            self.curves = self
                .surface
                .iter()
                .cloned()
                .chain(self.face(subject, eye))
                .chain(extract::suggestive(subject, self.labels.as_ref(), eye, &self.settings))
                .chain(extract::silhouettes(subject, eye))
                .collect();
            self.drawn_from = Some(eye);

            // Occlusion is no longer asked here. The drawing goes to the
            // GPU whole — every curve, unsplit — and the visibility
            // field decides which of its points carry width (ADR-0172).
            // What is left on this side is the lay-out and the pack:
            // extraction above, and the rail solve inside `solve`.
            if !self.strokes.solve(&self.curves, eye, self.view_matrix(), subject.surface_bias()) && self.strokes.live()
            {
                tracing::warn!(
                    target: "aether_puppet",
                    curves = self.curves.len(),
                    "the drawing does not fit the visibility field; no ink this frame",
                );
            }
        }

        // The easel, under everything above: develop when the view has
        // settled, then stand the sheet behind the subject. The develop
        // rasterizes its planes on the CPU and paints them through the
        // registered wash program (ADR-0170); the gate keeps the
        // rasterize off the frame cadence, and the presentation costs
        // one textured rect. It runs after the ribbons
        // rather than before because it reads them — the wash smears along
        // the drawing's own strokes, and the drawing has to be solved for
        // this eye first.
        let view = easel::View {
            eye,
            target: self.target(),
            view_proj: self.view_matrix(),
            aspect: self.aspect,
            field_of_view: FIELD_OF_VIEW,
        };
        if let Some(scores) = self.class_scores.as_ref() {
            // The wash bakes off the fine mesh — the one the ink plants on.
            // Baked off the coarse silhouette mesh instead, the mask's edge
            // disagrees with the drawn contour by a few pixels at a tight
            // silhouette, and a wash concentrates pigment at its mask's
            // edge, so the disagreement rendered as flush tracing the ears'
            // outline (issue 4399). Paint in the lines requires the mask
            // and the lines to agree about where the surface ends.
            let painted_mesh = subject;
            let chart = self.anchors.as_ref().zip(self.settings.face).map(|(anchors, face)| easel::Chart {
                mesh: subject,
                anchors,
                face,
            });
            // The CPU split, deferred behind the easel's own gate: the
            // wash smears along the drawing's strokes, so it wants the
            // visible runs — and it wants them a hundred times less
            // often than the frame does.
            let curves = &self.curves;
            let split = || Self::split(subject, curves, eye);
            let painted = easel::Subject { mesh: painted_mesh, scores, settings: &self.settings, drawn: &split, chart };

            self.easel.develop(&painted, &view, self.dragging);
        }

        // The easel's mail for this frame, in dependency order: the
        // program register (once per session), the destroys a resize
        // owes, the creates carrying a first develop at this size, then
        // the updates, the ribbon geometry the ink pass rasterizes, and
        // the dispatch that reads them all — to the same mailbox, so the
        // render cap sees them in exactly this order.
        if let Some(register) = self.easel.take_register() {
            self.awaiting.registers.push_back(Awaiting::Easel);
            render.send(register);
        }
        for destroy in self.easel.take_destroys() {
            render.send(&destroy);
        }
        for create in self.easel.take_creates() {
            self.awaiting.textures.push_back(Awaiting::Easel);
            render.send(&create);
        }
        for update in self.easel.take_updates() {
            render.send(&update);
        }
        if let Some(create) = self.easel.take_ink_create() {
            self.awaiting.geometries.push_back(Awaiting::Easel);
            render.send(&create);
        }
        if let Some(update) = self.easel.take_ink_update() {
            render.send(&update);
        }
        if let Some(dispatch) = self.easel.take_dispatch() {
            render.send(&dispatch);
        }
        let subject_radius = (subject.max - subject.min).length() * 0.5;
        if let Some(sheet) = self.easel.draw(&view, subject_radius) {
            render.send(&sheet);
        }

        // The ink's own mail, in the same dependency order and after
        // the wash's — the sheet draws in the material pass and the ink
        // composites in the overlay pass, so the ink lands on top by
        // the pass order rather than by anything sent here.
        for register in self.strokes.take_registers() {
            self.awaiting.registers.push_back(Awaiting::Strokes);
            render.send(&register);
        }
        for destroy in self.strokes.take_destroys() {
            render.send(&destroy);
        }
        for create in self.strokes.take_creates() {
            self.awaiting.textures.push_back(Awaiting::Strokes);
            render.send(&create);
        }
        for create in self.strokes.take_geometry_creates() {
            self.awaiting.geometries.push_back(Awaiting::Strokes);
            render.send(&create);
        }
        for update in self.strokes.take_geometry_updates() {
            render.send(&update);
        }
        for dispatch in self.strokes.take_dispatches() {
            render.send(&dispatch);
        }
        if let Some(ink) = self.strokes.present() {
            render.send(&ink);
        }
    }

    /// The drawing split into visible runs and ribboned, on the CPU.
    ///
    /// The frame's ink no longer comes from here — the stroke program
    /// rasterizes it from the visibility field — but the wash's flow
    /// bake still reads triangles, and the parity gates still compare
    /// against this. Tone gating already happened at load, so all that
    /// is asked per call is occlusion, and always of the fine mesh:
    /// what stands in front of a stroke is a question about the real
    /// surface.
    fn split(subject: &Mesh, curves: &[Curve3], eye: Vec3) -> Vec<DrawTriangle> {
        let mut triangles = Vec::new();
        for curve in curves {
            for run in visibility::runs(
                subject,
                eye,
                curve,
                &|_| true,
                visibility::Mode::Opaque,
                VISIBILITY_STRIDE,
                subject.surface_bias(),
            ) {
                ribbon::ribbon(&run, eye, 0, &mut triangles);
            }
        }

        triangles
    }

    /// The render cap answered the easel's program register. `Err` is
    /// the headless chassis' fail-fast reply (or a validation refusal),
    /// and it switches the easel off for the session rather than letting
    /// it ask again every settle.
    #[handler::single]
    fn on_program_registered(&mut self, _ctx: &mut WasmCtx<'_>, result: ProgramRegisterResult) {
        let result = match result {
            ProgramRegisterResult::Ok { program_id } => Ok(program_id),
            ProgramRegisterResult::Err { reason } => {
                tracing::info!(target: "aether_puppet", reason = %reason, "layer disabled: program register refused");
                Err(())
            }
        };
        match self.awaiting.registers.pop_front() {
            Some(Awaiting::Easel) => self.easel.registered(result),
            Some(Awaiting::Strokes) => self.strokes.registered(result),
            None => {}
        }
    }

    /// The render cap answered one of the easel's creates. `Err` is the
    /// headless chassis' fail-fast reply, and it switches the easel off
    /// for the session rather than letting it ask again every settle.
    #[handler::single]
    fn on_texture_created(&mut self, _ctx: &mut WasmCtx<'_>, result: CreateTextureResult) {
        let result = match result {
            CreateTextureResult::Ok { texture_id } => Ok(texture_id),
            CreateTextureResult::Err { error } => {
                tracing::info!(target: "aether_puppet", error = %error, "layer disabled: create_texture refused");
                Err(())
            }
        };
        match self.awaiting.textures.pop_front() {
            Some(Awaiting::Easel) => self.easel.created(result),
            Some(Awaiting::Strokes) => self.strokes.created(result),
            None => {}
        }
    }

    /// The render cap answered the easel's ribbon-geometry create. `Err`
    /// switches the easel off for the session, as a refused plane create
    /// does — the ink pass is declared in the graph, so a develop without
    /// its geometry has nothing to dispatch.
    #[handler::single]
    fn on_geometry_created(&mut self, _ctx: &mut WasmCtx<'_>, result: CreateGeometryResult) {
        let result = match result {
            CreateGeometryResult::Ok { geometry_id } => Ok(geometry_id),
            CreateGeometryResult::Err { reason } => {
                tracing::info!(target: "aether_puppet", reason = %reason, "layer disabled: create_geometry refused");
                Err(())
            }
        };
        match self.awaiting.geometries.pop_front() {
            Some(Awaiting::Easel) => self.easel.ink_created(result),
            Some(Awaiting::Strokes) => self.strokes.geometry_created(result),
            None => {}
        }
    }
}

aether_actor::export!(Puppet);
