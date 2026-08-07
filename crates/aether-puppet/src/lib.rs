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
//! her nose itself) and the suggestive contours — everything that depends
//! on where the eye is, and nothing else. Occlusion is not among them any
//! more: it is a field the GPU derives per frame (ADR-0172). The
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
pub mod deform;
pub mod easel;
pub mod extract;
pub mod feature;
mod gpu_silhouette;
pub mod idle;
mod kinds;
pub mod labels;
pub mod math3;
pub mod mesh;
mod npy;
pub mod plant;
pub mod ribbon;
#[allow(dead_code, reason = "the CPU half is the committed byte-and-order oracle for the live GPU consumer")]
mod silhouette;
pub mod strokes;
pub mod style;
pub mod turntable;
pub mod visibility;
pub mod weld;

pub use idle::*;
pub use kinds::*;
pub use labels::MaterialField;
pub use turntable::*;

use aether_actor::{ActorInitError, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_fs::{FsCapability, FsMailboxExt, ReadResult};
use aether_kinds::{MouseButton, MouseButtonRelease, MouseMove, MouseWheel, Render, WindowSize};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::{Mat4, Vec2, Vec3};
use aether_render::{
    CreateGeometryResult, CreateTextureResult, ProgramRegisterResult, RenderCapability, ViewProjection,
};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};
use core::mem;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use easel::palette::Palette;
use feature::{Curve3, Drawing};
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

/// Drag sensitivity. A full sweep of a 900-pixel window turns her a bit
/// more than half a revolution, which is about where a drag stops feeling
/// like shoving and starts feeling like turning.
const ORBIT_DEGREES_PER_PIXEL: f32 = 0.25;

/// Fraction of the current distance one wheel notch covers.
const DOLLY_PER_NOTCH: f32 = 0.08;

/// Interactive dolly bounds. The far bound also sizes the sight field's
/// reach scan: its point window must still cover the angular pressure ramp
/// when the subject is smallest on screen.
const MIN_DOLLY_DISTANCE: f32 = 0.6;
const MAX_DOLLY_DISTANCE: f32 = 40.0;

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

/// Diagnostic selector for the resident GPU silhouette candidate.
///
/// The default (`enabled: false`) is the current CPU silhouette. The
/// exceptional overlay draws only curves that touched a non-manifold
/// junction, in a high-contrast inspection colour; it is evidence, not a
/// shipping style.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.gpu_silhouette_mode")]
pub struct GpuSilhouetteMode {
    pub enabled: bool,
    pub exceptional_overlay: bool,
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
    /// The field's bytes, held until the box that has to paint them is in.
    ///
    /// A cell names its class by position in a vocabulary, and the
    /// vocabulary belongs to the palette — so a field cannot be decoded
    /// until the box has settled, and the two reads settle in either
    /// order. Held rather than parsed, the way the rig's weights are.
    staged_labels: Option<Vec<u8>>,
    /// The painter's box this subject is painted out of, and the path
    /// still owed for it. The canonical box until a load names its own.
    palette: Palette,
    awaiting_palette: Option<String>,
    /// Padding declared by the load that requested `awaiting_labels`.
    material_field_padding: f32,
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
    /// Last frame's view-dependent curves, and the frame they were solved
    /// for: the silhouette, the charted face and the suggestive contours.
    /// The other half of the drawing is `surface` — until a pose moves it
    /// here too, which is what the split by volatility is for.
    ///
    /// Silhouette and visibility are the only view-dependent work, so a
    /// frame drawn from an eye we have already solved is the same frame.
    /// The window redraws continuously whether or not anything moved, and
    /// without this every one of those redraws re-marched 868k faces and
    /// re-cast every occlusion ray to arrive at the identical answer.
    volatile: Vec<Curve3>,
    drawn_from: Option<Frame>,
    /// Held until extraction runs, because the answer is not known until
    /// both assets have landed.
    owed: Option<ReplyHandle>,
    /// The view-independent drawing: hatch and crease, welded, kept from
    /// load. Rebuilt only when the subject changes, which is what lets
    /// its packed field points stay resident on the GPU across an orbit
    /// (iamacoffeepot/aether#4435).
    surface: Vec<Curve3>,
    /// The rig this subject was loaded with, and what it is currently
    /// doing. Without a rig the pose is inert and every frame is the rest
    /// frame.
    skin: Option<deform::Skin>,
    /// Staging for the rig's two files, which settle independently of each
    /// other and of the mesh they have to be checked against.
    rig: Rig,
    pose: Pose,
    /// The subject skinned to `posed_at`. Not read at rest, where the
    /// sculpt itself *is* the posed surface; allocated once with the rig
    /// so a pose writes into it rather than reallocating a mesh per
    /// frame.
    ///
    /// One consumer remains, and it is the one a pose cannot be cached
    /// through: the silhouette is the zero set of `view . normal` and
    /// depends on the pose *and* the eye, so it is genuinely re-extracted
    /// off a posed surface. Everything else the pose used to be carried
    /// onto — the surface curves, their tone gate, the prepass depth,
    /// the wash's own subject — is posed in a vertex stage now, from a
    /// bone table that rides the uniform blob
    /// (iamacoffeepot/aether#4462).
    posed: Option<Mesh>,
    posed_at: Option<Pose>,
    /// Where each bone sends a point at `posed_at`, and the same table as
    /// the uniform lanes every vertex stage skins from. Empty at rest,
    /// where every bone is the identity.
    transforms: Vec<deform::Rigid>,
    bones: [f32; deform::BONE_LIMIT * 12],
    settings: extract::Settings,
    look: Look,
    /// The wash layer under the ink (#4349): a painted sheet standing
    /// behind the subject, re-developed when the view settles.
    easel: easel::Easel,
    /// The ink layer over it (ADR-0172): the visibility field and the
    /// stroke program, re-solved whenever the eye moves.
    strokes: strokes::Strokes,
    /// The instrument-only resident silhouette candidate. The CPU path
    /// remains the default and oracle until its captures are approved.
    gpu_silhouette: gpu_silhouette::GpuSilhouette,
    /// Which layer each in-flight reply belongs to. The easel and the
    /// ink both register programs and create textures and geometry, and
    /// the replies carry no sender — so the answer is position: the
    /// render cap replies to one kind in the order that kind was sent.
    awaiting: Awaiting3,
}

/// Everything the cached drawing is a function of.
///
/// Keyed on the eye alone this was sound only while the mesh was static:
/// once a pose exists the eye can be unchanged while the drawing is not,
/// and the cache would serve a stale frame. So the key carries whatever
/// produced the geometry, not just whatever produced the view
/// (iamacoffeepot/aether#4336).
#[derive(Clone, Copy, PartialEq)]
struct Frame {
    eye: Vec3,
    pose: Pose,
}

/// The rig's two files, held until the mesh they describe is in.
///
/// They arrive as separate mail in whichever order the reads settle, and
/// the weights cannot be checked against a vertex count that has not
/// landed yet — so the bytes wait here and the rig is built once nothing
/// is outstanding, exactly as the material field waits for its bounds.
#[derive(Default)]
struct Rig {
    weights: Option<Vec<u8>>,
    descriptor: Option<String>,
    awaiting_weights: Option<String>,
    awaiting_descriptor: Option<String>,
}

impl Rig {
    /// Whether `path` is one of the rig's two outstanding reads.
    fn claims(&self, path: &str) -> bool {
        [&self.awaiting_weights, &self.awaiting_descriptor].into_iter().any(|owed| owed.as_deref() == Some(path))
    }

    fn accept(&mut self, path: &str, bytes: Vec<u8>) {
        if self.awaiting_weights.as_deref() == Some(path) {
            self.awaiting_weights = None;
            self.weights = Some(bytes);
        } else {
            self.awaiting_descriptor = None;
            self.descriptor = String::from_utf8(bytes).ok();
        }
    }

    fn outstanding(&self) -> bool {
        self.awaiting_weights.is_some() || self.awaiting_descriptor.is_some()
    }

    fn build(&self, vertices: usize) -> Result<Option<deform::Skin>, String> {
        match (&self.weights, &self.descriptor) {
            (None, None) => Ok(None),
            (Some(weights), Some(descriptor)) => deform::Skin::parse(weights, descriptor, vertices).map(Some),
            (Some(_), None) => Err("rig descriptor is missing or is not valid UTF-8".to_owned()),
            (None, Some(_)) => Err("rig weights are missing".to_owned()),
        }
    }
}

/// The layer an in-flight ask belongs to.
#[derive(Clone, Copy)]
enum Awaiting {
    Easel,
    GpuSilhouette,
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

    /// Invalidate only what chart state contributes to. Surface extraction,
    /// the material survey, and resident GPU geometry remain valid.
    fn chart_changed(&mut self) {
        self.drawn_from = None;
        self.easel.chart_changed();
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

    /// Where the eye stands in her own frame.
    ///
    /// The chart is authored in the model's frontal plane and planted
    /// along it, so a head that has turned is drawn by carrying the viewer
    /// back through the head's rotation rather than by planting a face on
    /// a turned head — which casts every ray at a graze and slides the
    /// marks across her cheek as she moves. It is also the frame the turn
    /// gate wants: "is the profile over her bridge yet" is a question
    /// about where the eye is *relative to her*.
    fn charting_eye(&self, eye: Vec3) -> Vec3 {
        self.skin.as_ref().filter(|_| !self.pose.is_rest()).map_or(eye, |_| self.head_at_pose().inverse().point(eye))
    }

    /// Where the head bone sends a point at this pose, and the identity
    /// while nothing is driving it.
    ///
    /// The wash's accents want this rather than the whole bone table: an
    /// eye is bound wholly to the head, so its blend at every point is
    /// the head's own map, and the paint reaches the pose by one
    /// transform of a couple of dozen planted points.
    fn head_at_pose(&self) -> deform::Rigid {
        self.skin
            .as_ref()
            .filter(|_| !self.pose.is_rest())
            .map_or(deform::Rigid::IDENTITY, |skin| skin.head(&self.pose))
    }

    /// Bring the posed surface up to the current pose, if it is not there
    /// already.
    ///
    /// One skinning pass now, where there were two, and it survives for
    /// the one consumer a pose cannot be cached through: the silhouette
    /// is re-extracted off the posed surface every frame because it is
    /// the zero set of `view . normal`, so it wants posed positions and
    /// posed normals on this side. The curve pass that stood in for
    /// re-solving hatch and crease is gone — those curves are packed
    /// against their anchorages and posed in the vertex stage, which is
    /// what stopped a posed frame from shipping the whole drawing.
    ///
    /// The bone table is the pose's whole footprint on everything else:
    /// the prepass depth, the ink's rails and the wash's own subject all
    /// read it out of their dispatch's uniform blob.
    fn repose(&mut self) {
        if self.posed_at == Some(self.pose) {
            return;
        }
        let (Some(skin), Some(subject), Some(posed)) = (self.skin.as_ref(), self.subject.as_ref(), self.posed.as_mut())
        else {
            return;
        };
        self.posed_at = Some(self.pose);

        self.transforms = if self.pose.is_rest() {
            Vec::new()
        } else {
            skin.transforms(&self.pose)
        };
        self.bones = deform::bone_uniform(&self.transforms);
        if self.pose.is_rest() {
            return;
        }

        skin.pose_surface(&self.transforms, subject, &mut posed.positions, &mut posed.normals);
        posed.rebound(&self.transforms);
    }

    /// Everything the frame's drawing is: the surface posed, the curves
    /// carried onto it, and the view-dependent features re-solved.
    fn resolve(&mut self, frame: Frame) {
        self.repose();

        let view_proj = self.view_matrix();
        let charting_eye = self.charting_eye(frame.eye);
        let subject = self.subject.as_ref().expect("a frame is only resolved once a subject is in");
        let posed = !self.pose.is_rest() && self.posed.is_some();
        let surface = self.posed.as_ref().filter(|_| posed).unwrap_or(subject);
        let bias = surface.surface_bias();

        // The face rides the per-eye path rather than the cached surface,
        // because the nose bar retires once her face turns over its own
        // bridge and that is a question about where the eye is. Suggestive
        // contours are there for the same reason — they are the silhouette
        // one derivative out.
        let mut face = self.face(subject, charting_eye);
        if let (Some(skin), true) = (self.skin.as_ref(), posed) {
            skin.pose_curves(&self.transforms, surface, &mut face);
        }

        // The volatile half is solved on the CPU at whatever pose is
        // running and so arrives already posed — a few hundred charted
        // points carried onto the surface above, and a silhouette
        // extracted off it. It is the resident half that a pose used to
        // move here wholesale, and no longer does.
        self.volatile = face
            .into_iter()
            .chain(extract::suggestive(surface, subject, self.labels.as_ref(), frame.eye, &self.settings))
            .chain(
                (!self.gpu_silhouette.selected())
                    .then(|| extract::silhouettes(surface, frame.eye))
                    .into_iter()
                    .flatten(),
            )
            .collect();
        self.drawn_from = Some(frame);

        if self.gpu_silhouette.selected() {
            self.gpu_silhouette.solve(view_proj, frame.eye, &self.bones);
        }

        // A pose no longer costs the drawing its residency. Every surface
        // curve is packed against the anchorage it was extracted at and
        // posed in the vertex stage, so the buffer #4435 left on the GPU
        // across an orbit is the same buffer a pose sweep leaves there —
        // and what travels per frame is the volatile minority and a bone
        // table (iamacoffeepot/aether#4462).
        //
        // Occlusion is not asked here either. The drawing goes to the GPU
        // whole — every curve, unsplit — and the visibility field decides
        // which of its points carry width (ADR-0172). What is left on this
        // side is the lay-out and the pack: extraction above, and the rail
        // solve inside `solve`.
        //
        // `bound` is `Some` only for a rigged subject, and it decides two
        // things at once: what the resident curves are packed against, so
        // their vertex stage can pose them, and whether the tone gate
        // runs on the GPU at all — an unrigged subject's curves were
        // gated at load, against normals nothing turns.
        let posing = strokes::Posing {
            bound: self.skin.as_ref().map(|skin| deform::Bound { rest: subject, skin }),
            bones: self.bones,
            tone: easel::program::sight::ToneUniforms::of(&self.settings, self.skin.is_some()),
        };
        let drawing = Drawing { resident: &self.surface, volatile: &self.volatile };
        if !self.strokes.solve(drawing, frame.eye, view_proj, bias, self.aspect, posing) && self.strokes.live() {
            tracing::warn!(
                target: "aether_puppet",
                curves = drawing.len(),
                "the drawing does not fit the visibility field; no ink this frame",
            );
        }
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
            staged_labels: None,
            palette: Palette::canonical(),
            awaiting_palette: None,
            material_field_padding: DEFAULT_MATERIAL_FIELD_PADDING,
            anchors: None,
            dragging: false,
            cursor: Vec2::new(0.0, 0.0),
            aspect: ASPECT_UNTIL_MEASURED,
            volatile: Vec::new(),
            drawn_from: None,
            owed: None,
            surface: Vec::new(),
            skin: None,
            rig: Rig::default(),
            pose: Pose::default(),
            posed: None,
            posed_at: None,
            transforms: Vec::new(),
            bones: deform::bone_uniform(&[]),
            settings: extract::Settings::default(),
            easel: easel::Easel::default(),
            strokes: strokes::Strokes::default(),
            gpu_silhouette: gpu_silhouette::GpuSilhouette::default(),
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
    ///
    /// An aspect that moved re-solves the drawing, by the eye it was
    /// already drawn from. Nothing extracted here is a function of the
    /// aspect, but the matrix every layer projects through is, and the
    /// ink's is held from the solve that laid it down rather than
    /// rebuilt per frame — so without this a window dragged while the
    /// camera sits still re-fills the field through the projection of
    /// the shape it used to be.
    ///
    /// Only when it moved. A desktop window republishes its size on
    /// every redraw, so this handler runs per frame at a steady size —
    /// and a re-solve keyed on the announcement rather than on the
    /// change would put the whole extraction back on every frame of a
    /// held camera, which is the cost the eye check exists to avoid.
    #[handler::single]
    fn on_window_size(&mut self, _ctx: &mut WasmCtx<'_>, size: WindowSize) {
        if size.width > 0 && size.height > 0 {
            let aspect = size.width as f32 / size.height as f32;
            self.easel.resized(size.width, size.height);
            self.strokes.resized(size.width, size.height);
            self.gpu_silhouette.resized(size.width, size.height);
            if self.aspect != aspect {
                self.aspect = aspect;
                self.drawn_from = None;
            }
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
        self.look.distance = (self.look.distance * (1.0 - wheel.delta_y * DOLLY_PER_NOTCH))
            .clamp(MIN_DOLLY_DISTANCE, MAX_DOLLY_DISTANCE);
    }

    /// Point her at a subject. Asynchronous — the reply target is carried
    /// in the fs context so the eventual `LoadResult` reaches whoever asked.
    #[handler::manual]
    fn on_load(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: Load) {
        self.owed = ctx.reply_target();
        self.material_field_padding = mail.material_field_padding;

        let fetch = |path: String| {
            let context = LoadContext { reply: None, namespace: mail.namespace.clone(), path };
            ctx.actor::<FsCapability>().with_context(&context).read(&context.namespace, &context.path);
        };

        // The box first, because the field is read against it: a load
        // that names no palette paints out of the canonical box, and one
        // that names a box it cannot read falls back to the same rather
        // than painting out of the last subject's.
        self.palette = Palette::canonical();
        if !mail.palette.is_empty() {
            self.awaiting_palette = Some(mail.palette.clone());
            fetch(mail.palette.clone());
        }
        if !mail.labels.is_empty() {
            self.awaiting_labels = Some(mail.labels.clone());
            fetch(mail.labels.clone());
        }
        if !mail.rig.is_empty() {
            self.skin = None;
            self.rig = Rig {
                awaiting_weights: Some(format!("{}/weights.npy", mail.rig)),
                awaiting_descriptor: Some(format!("{}/rig.txt", mail.rig)),
                ..Rig::default()
            };
            fetch(format!("{}/weights.npy", mail.rig));
            fetch(format!("{}/rig.txt", mail.rig));
        }

        fetch(mail.path.clone());
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

    /// File one settled read against the asset that owed it, decided by
    /// the path the reply echoes rather than by arrival order — the reads
    /// settle independently of each other.
    ///
    /// `Err` is the one refusal that ends a load: without a mesh there is
    /// nothing to draw. A box or a field that will not decode degrades
    /// instead and says so in the log, because a subject painted out of
    /// the canonical box is still a subject.
    fn accept(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), String> {
        if self.rig.claims(path) {
            // Held rather than parsed: the weights are only meaningful
            // against a vertex count, and the mesh may not be in yet.
            self.rig.accept(path, bytes);
        } else if self.awaiting_palette.as_deref() == Some(path) {
            self.awaiting_palette = None;
            match String::from_utf8(bytes)
                .map_err(|_| "palette is not valid UTF-8".to_owned())
                .and_then(|text| Palette::decode_text(&text))
            {
                Ok(palette) => self.palette = palette,
                Err(error) => tracing::warn!(
                    target: "aether_puppet",
                    path = %path,
                    error = %error,
                    "palette refused; painting out of the canonical box",
                ),
            }
        } else if self.awaiting_labels.as_deref() == Some(path) {
            // Held rather than decoded: the cells name classes by position
            // in the box's vocabulary, and neither the box nor the mesh
            // the lattice is placed against need have settled yet.
            self.awaiting_labels = None;
            self.staged_labels = Some(bytes);
        } else {
            let Some(subject) = Mesh::from_obj_bytes(&bytes, self.settings.relaxation) else {
                tracing::warn!(target: "aether_puppet", "parse failed; keeping the previous subject");
                return Err(format!("{path} is not a mesh this reader accepts"));
            };
            self.subject = Some(subject);
        }

        Ok(())
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

        if let Err(reason) = self.accept(&path, bytes) {
            self.settle(ctx, &LoadResult::Err { reason });
            return;
        }

        // Extraction needs the mesh, and the field if one was asked for —
        // the lattice is placed against the mesh's own bounds, so a field
        // that lands first has to be re-placed once the mesh arrives.
        let Some(subject) = self.subject.as_ref() else {
            return;
        };
        if self.awaiting_labels.is_some() || self.awaiting_palette.is_some() || self.rig.outstanding() {
            return;
        }

        // Everything is in, so the field can finally be read: against the
        // box's vocabulary, which says what its cells name, and against
        // the mesh's own bounds, which say where its lattice sits.
        let (min, max) = (subject.min, subject.max);
        if let Some(bytes) = self.staged_labels.take() {
            match MaterialField::decode(&bytes, self.palette.classes(), min, max, self.material_field_padding) {
                Ok(labels) => self.labels = Some(labels),
                Err(error) => {
                    self.labels = None;
                    tracing::warn!(
                        target: "aether_puppet",
                        error = %error,
                        "material field refused; creases stay unmasked",
                    );
                }
            }
        }

        // A field kept from an earlier load has a lattice placed against
        // that subject's bounds, and those scale the whole thing — so it
        // is re-placed against this one (issue 4401).
        if let Some(labels) = self.labels.as_mut() {
            labels.place_against(min, max, self.material_field_padding);
        }

        // Where her features are, measured off the field before anything is
        // drawn on them. Placement is derived; only the shape is authored.
        self.anchors = self.labels.as_ref().and_then(|labels| anchor::Anchors::measure(subject, labels));
        self.class_scores = self.labels.as_ref().map(|labels| labels.vertex_scores(&subject.positions));

        // Hatch and crease describe the surface, not the view, so they are
        // solved once here rather than every frame.
        self.surface = extract::surface(subject, self.labels.as_ref(), self.anchors.as_ref(), &self.settings);

        // The rig, checked against the mesh it claims to bind. A refused
        // one leaves her unposable rather than posing her wrongly, and the
        // reply says so.
        match self.rig.build(subject.positions.len()) {
            Ok(skin) => self.skin = skin,
            Err(error) => {
                self.skin = None;
                tracing::warn!(
                    target: "aether_puppet",
                    error = %error,
                    "the rig was refused; she stays in the rest pose",
                );
            }
        }

        // Ungated when a rig is in: the gate reads each point's normal and
        // skinning turns normals, so it moves to the vertex stage that
        // poses them (`sight.wgsl`'s `hatched`). With no rig nothing
        // turns, and the load-time gate is the whole of it — which is why
        // the shader's own gate is switched off for that subject rather
        // than asked to agree with this one.
        if self.skin.is_none() {
            self.surface = extract::tone_gate(mem::take(&mut self.surface), &self.settings);
        }
        self.posed = self.skin.as_ref().map(|skin| subject.deformable(skin));
        // A fresh subject stands at rest, and `subject_changed` below has
        // already staged it, so the first frame owes no skinning at all.
        self.posed_at = Some(Pose::default());
        self.pose = Pose::default();
        self.transforms = Vec::new();
        self.bones = deform::bone_uniform(&[]);
        self.drawn_from = None;
        self.easel.subject_changed();
        self.strokes.subject_changed(subject, self.skin.as_ref());
        if self.gpu_silhouette.selected()
            && let Err(error) = self.gpu_silhouette.subject_changed(subject, self.skin.as_ref())
        {
            tracing::warn!(target: "aether_puppet", error = %error, "GPU silhouette candidate refused this subject");
        }
        tracing::info!(
            target: "aether_puppet",
            faces = subject.faces.len(),
            curves = self.surface.len(),
            masked = self.labels.is_some(),
            bones = self.skin.as_ref().map_or(0, deform::Skin::bones),
            eyes = self.anchors.as_ref().map_or(0, |at| at.eyes.len()),
            "subject loaded",
        );

        let settled = LoadResult::Ok {
            vertices: subject.positions.len() as u32,
            faces: subject.faces.len() as u32,
            bones: self.skin.as_ref().map_or(0, deform::Skin::bones) as u32,
        };
        self.settle(ctx, &settled);
    }

    /// Drive the rig. Fire-and-forget: the pose is state, and the frame
    /// that follows draws it.
    #[handler::single]
    fn on_pose(&mut self, _ctx: &mut WasmCtx<'_>, pose: Pose) {
        self.pose = pose;
    }

    /// Switch between the shipping CPU oracle and the instrument-only GPU
    /// candidate. Selection invalidates the view solve so the two paths can
    /// never share geometry from different frames.
    #[handler::single]
    fn on_gpu_silhouette_mode(&mut self, _ctx: &mut WasmCtx<'_>, mode: GpuSilhouetteMode) {
        let mounting = mode.enabled && !self.gpu_silhouette.selected();
        if self.gpu_silhouette.select(mode.enabled, mode.exceptional_overlay) {
            self.drawn_from = None;
        }
        if mounting
            && let Some(subject) = self.subject.as_ref()
            && let Err(error) = self.gpu_silhouette.subject_changed(subject, self.skin.as_ref())
        {
            tracing::warn!(target: "aether_puppet", error = %error, "GPU silhouette candidate refused this subject");
        }
    }

    /// Choose a named face while preserving the direction she is looking.
    ///
    /// # Agent
    /// Send one of `rest`, `happy`, `grin`, `angry`, `surprised`, `smug`,
    /// `sad`, or `speaking`. Unknown names leave the current face unchanged.
    #[handler::single]
    fn on_expression(&mut self, _ctx: &mut WasmCtx<'_>, expression: Expression) {
        if set_expression(&mut self.settings, &expression.name) {
            self.chart_changed();
        } else if chart::face(&expression.name).is_none() {
            tracing::warn!(target: "aether_puppet", name = %expression.name, "unknown expression");
        }
    }

    /// Move both irises together, with the lids following vertically.
    ///
    /// # Agent
    /// `x` and `y` are normalized axes clamped to `[-1, 1]`; positive `x`
    /// is toward her left and positive `y` is up. Non-finite values are ignored.
    #[handler::single]
    fn on_gaze(&mut self, _ctx: &mut WasmCtx<'_>, gaze: Gaze) {
        if set_gaze(&mut self.settings, gaze) {
            self.chart_changed();
        } else if !gaze.x.is_finite() || !gaze.y.is_finite() {
            tracing::warn!(target: "aether_puppet", x = gaze.x, y = gaze.y, "non-finite gaze ignored");
        }
    }

    /// Replace only the mouth shape, leaving expression, gaze, and eye design.
    ///
    /// # Agent
    /// Speech shapes are `closed`, `A`, `I`, `U`, `E`, and `O`. The chart's
    /// expression shapes `rest`, `smile`, `grin`, `frown`, `smirk`, and `pout`
    /// are accepted too. Unknown names leave the current mouth unchanged.
    #[handler::single]
    fn on_viseme(&mut self, _ctx: &mut WasmCtx<'_>, viseme: Viseme) {
        if set_viseme(&mut self.settings, &viseme.name) {
            self.chart_changed();
        } else if chart::mouth::shape(&viseme.name).is_none() {
            tracing::warn!(target: "aether_puppet", name = %viseme.name, "unknown viseme");
        }
    }

    /// Choose the eye design without changing expression, gaze, or speech.
    ///
    /// # Agent
    /// Send one of `kitsune`, `vulpine`, `sketch`, `cool`, `soft`, `wide`, or
    /// `mask`. Unknown names leave the current eye design unchanged.
    #[handler::single]
    fn on_eye_archetype(&mut self, _ctx: &mut WasmCtx<'_>, archetype: EyeArchetype) {
        if set_eye_archetype(&mut self.settings, &archetype.name) {
            self.chart_changed();
        } else {
            tracing::warn!(target: "aether_puppet", name = %archetype.name, "unknown eye archetype");
        }
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
        let painted = easel::Subject {
            mesh: subject,
            posed: self.posed.as_ref().filter(|_| !self.pose.is_rest()),
            scores,
            palette: &self.palette,
            settings: &self.settings,
            ink: self.strokes.ink_plane(),
            chart: None,
            skin: self.skin.as_ref(),
            bones: self.bones,
        };
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
    #[allow(clippy::too_many_lines, reason = "one ordered render mailbox transaction across the three layers")]
    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _stage: Render) {
        let render = ctx.actor::<RenderCapability>();
        render.send(&self.view_projection());

        if self.subject.is_none() {
            return;
        }

        let eye = self.eye();
        let frame = Frame { eye, pose: self.pose };
        if self.drawn_from != Some(frame) {
            self.resolve(frame);
        }
        let subject = self.subject.as_ref().expect("a subject that was there a moment ago");

        // The easel, under everything above: develop this view, then stand
        // the sheet behind the subject. Every frame — the whole develop is
        // two registered dispatches now (ADR-0170/0171), and nothing left
        // on the CPU scales with the canvas — and the presentation costs
        // one textured rect. It runs after the ribbons rather than before
        // because it reads them: the wash smears along the drawing's own
        // strokes, and the drawing has to be solved for this eye first.
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
            //
            // The *rest* fine mesh, and it stays the rest one under a
            // pose: the bake's vertex stage poses it from the bone table
            // below, so the buffer uploaded once per subject is still the
            // right buffer and the wash's mask lands on the same pose the
            // ink does. The alternative was a subject re-upload path,
            // which would have been the per-frame whole-buffer upload
            // iamacoffeepot/aether#4462 exists to delete.
            //
            // What still stands at rest is the develop's own per-frame
            // terms — the material centroids, the chart's eye frames, the
            // aperture cast — all of which are measured off the sculpt.
            // They drift by the pose rather than by the frame, which is a
            // smaller and separable gap than a mask on the wrong pose.
            let painted_mesh = subject;
            let chart = self.anchors.as_ref().zip(self.settings.face).map(|(anchors, face)| easel::Chart {
                mesh: subject,
                anchors,
                face,
                head: self.head_at_pose(),
                eye: self.charting_eye(eye),
            });
            // Where the ink stands, named rather than re-derived: the
            // stroke program reduced it out of the very raster it drew
            // the frame's ink from, one dispatch ago in this same handler
            // (iamacoffeepot/aether#4451).
            let ink = self.strokes.ink_plane();
            let painted = easel::Subject {
                mesh: painted_mesh,
                posed: self.posed.as_ref().filter(|_| !self.pose.is_rest()),
                scores,
                palette: &self.palette,
                settings: &self.settings,
                ink,
                chart,
                skin: self.skin.as_ref(),
                bones: self.bones,
            };

            self.easel.develop(&painted, &view);
        }

        // The ink's own mail first, in dependency order: the register,
        // the texture destroys a resize owes, the creates, the geometry
        // creates and the updates the eye moved, then the field's
        // dispatch and the ink's. A program's passes record in dispatch
        // arrival order, and the ink's last pass but one writes the
        // coverage plane the wash below binds — so this order is what
        // makes the paint read this frame's drawing rather than the
        // frame before's.
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

        // The candidate owns a separate transparent sheet and is mounted
        // only by its diagnostic selector. Its resident compute/draw graph
        // follows the ordinary ink so the CPU face and suggestive curves
        // keep their established order, with the replacement silhouette
        // composited last.
        for destroy in self.gpu_silhouette.take_program_destroys() {
            render.send(&destroy);
        }
        for register in self.gpu_silhouette.take_registers() {
            self.awaiting.registers.push_back(Awaiting::GpuSilhouette);
            render.send(&register);
        }
        for destroy in self.gpu_silhouette.take_destroys() {
            render.send(&destroy);
        }
        for create in self.gpu_silhouette.take_creates() {
            self.awaiting.textures.push_back(Awaiting::GpuSilhouette);
            render.send(&create);
        }
        for create in self.gpu_silhouette.take_geometry_creates() {
            self.awaiting.geometries.push_back(Awaiting::GpuSilhouette);
            render.send(&create);
        }
        for update in self.gpu_silhouette.take_geometry_updates() {
            render.send(&update);
        }
        for dispatch in self.gpu_silhouette.take_dispatches() {
            render.send(&dispatch);
        }

        // The easel's, in the same dependency order: the programs a
        // re-laid graph has finished with, the program registers (both at
        // the first develop, the wash's again after a re-lay), the
        // texture destroys a resize owes, the creates carrying this
        // canvas' resident planes, the geometry creates and the update
        // that moves with the eye, then the two dispatches that read them
        // all — to the same mailbox, so the render cap sees them in
        // exactly this order.
        for destroy in self.easel.take_program_destroys() {
            render.send(&destroy);
        }
        for register in self.easel.take_registers() {
            self.awaiting.registers.push_back(Awaiting::Easel);
            render.send(&register);
        }
        for destroy in self.easel.take_destroys() {
            render.send(&destroy);
        }
        for create in self.easel.take_creates() {
            self.awaiting.textures.push_back(Awaiting::Easel);
            render.send(&create);
        }
        for create in self.easel.take_geometry_creates() {
            self.awaiting.geometries.push_back(Awaiting::Easel);
            render.send(&create);
        }
        for update in self.easel.take_geometry_updates() {
            render.send(&update);
        }
        for dispatch in self.easel.take_dispatch() {
            render.send(&dispatch);
        }

        // The ordinary two billboards, sheet first, then the opt-in GPU
        // silhouette candidate. The material pass writes no depth, so they
        // compose in send order.
        let subject_radius = (subject.max - subject.min).length() * 0.5;
        if let Some(sheet) = self.easel.draw(&view, subject_radius) {
            render.send(&sheet);
        }
        if let Some(ink) = self.strokes.draw(&view, subject_radius) {
            render.send(&ink);
        }
        if let Some(silhouette) = self.gpu_silhouette.draw(&view, subject_radius) {
            render.send(&silhouette);
        }
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
            Some(Awaiting::GpuSilhouette) => self.gpu_silhouette.registered(result),
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
            Some(Awaiting::GpuSilhouette) => self.gpu_silhouette.created(result),
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
            Some(Awaiting::Easel) => self.easel.geometry_created(result),
            Some(Awaiting::GpuSilhouette) => self.gpu_silhouette.geometry_created(result),
            Some(Awaiting::Strokes) => self.strokes.geometry_created(result),
            None => {}
        }
    }
}

fn set_expression(settings: &mut extract::Settings, name: &str) -> bool {
    let Some(mut expression) = chart::face(name) else {
        return false;
    };
    expression.gaze = settings.face.map_or(Vec2::ZERO, |face| face.gaze);
    if settings.face == Some(expression) {
        return false;
    }
    settings.face = Some(expression);

    true
}

fn set_gaze(settings: &mut extract::Settings, gaze: Gaze) -> bool {
    if !gaze.x.is_finite() || !gaze.y.is_finite() {
        return false;
    }
    let Some(face) = settings.face.as_mut() else {
        return false;
    };
    let gaze = Vec2::new(gaze.x.clamp(-1.0, 1.0), gaze.y.clamp(-1.0, 1.0));
    if face.gaze == gaze {
        return false;
    }
    face.gaze = gaze;

    true
}

fn set_viseme(settings: &mut extract::Settings, name: &str) -> bool {
    let (Some(mouth), Some(face)) = (chart::mouth::shape(name), settings.face.as_mut()) else {
        return false;
    };
    if face.mouth == mouth {
        return false;
    }
    face.mouth = mouth;

    true
}

fn set_eye_archetype(settings: &mut extract::Settings, name: &str) -> bool {
    let Some(style) = chart::eye::style(name) else {
        return false;
    };
    settings.eye_style = style;

    true
}

#[cfg(test)]
mod control_tests {
    use super::*;

    #[test]
    fn expression_gaze_and_viseme_compose_without_resetting_each_other() {
        let mut settings = extract::Settings::default();

        assert!(set_gaze(&mut settings, Gaze { x: 1.8, y: -1.4 }));
        assert!(set_expression(&mut settings, "angry"));
        let angry = settings.face.expect("the expression keeps the chart enabled");
        assert_eq!(angry.gaze, Vec2::new(1.0, -1.0), "an expression must preserve the shared gaze");

        assert!(set_viseme(&mut settings, "A"));
        let speaking = settings.face.expect("the viseme keeps the chart enabled");
        assert_eq!(speaking.mouth, chart::mouth::shape("A").expect("the chart has the A viseme"));
        assert_eq!(speaking.brow, angry.brow, "a viseme must not replace the expression's brows");
        assert_eq!(speaking.eye, angry.eye, "a viseme must not replace the expression's aperture");
        assert_eq!(speaking.gaze, angry.gaze, "a viseme must not replace gaze");
    }

    #[test]
    fn invalid_controls_leave_the_current_chart_state_intact() {
        let mut settings = extract::Settings::default();
        assert!(set_expression(&mut settings, "happy"));
        assert!(set_gaze(&mut settings, Gaze { x: -0.4, y: 0.7 }));
        let before = settings.face;
        let eye_scale = settings.eye_style.scale;

        assert!(!set_expression(&mut settings, "missing"));
        assert!(!set_viseme(&mut settings, "missing"));
        assert!(!set_gaze(&mut settings, Gaze { x: f32::NAN, y: 0.0 }));
        assert!(!set_eye_archetype(&mut settings, "missing"));
        assert_eq!(settings.face, before);
        assert_eq!(settings.eye_style.scale, eye_scale);
    }

    #[test]
    fn eye_archetype_changes_only_the_style() {
        let mut settings = extract::Settings::default();
        assert!(set_expression(&mut settings, "smug"));
        assert!(set_gaze(&mut settings, Gaze { x: 0.5, y: -0.25 }));
        let face = settings.face;

        assert!(set_eye_archetype(&mut settings, "wide"));
        assert_eq!(settings.face, face);
        assert_eq!(settings.eye_style.scale, chart::eye::style("wide").expect("the wide style exists").scale);
    }
}

aether_actor::export!(Puppet, Idle, Turntable);
