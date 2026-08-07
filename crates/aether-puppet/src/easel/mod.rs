//! The easel: watercolour laid under the drawing
//! (iamacoffeepot/aether#4349).
//!
//! The wash follows changed frames. A moved render stage develops the
//! whole painting afresh — a sheet of painted paper standing behind the
//! subject, re-oriented to face the eye — while a held one keeps presenting
//! the sheet already standing. The ink is solved live above it and wins the
//! depth test, so the drawing and the paint under it move together through
//! an orbit rather than the paint lagging a held view behind
//! (iamacoffeepot/aether#4387).
//!
//! [`field`] turns a region into wet paint on the CPU — the parity
//! oracle — while [`program`] speaks the same develop as registered
//! ADR-0170/0171 render programs; [`palette`] owns the pigments;
//! [`regions`] keeps the CPU bake the oracle rasterizes through.
//! This module is the orchestrator: what is resident, what a changed frame
//! stages, and where the sheet stands.
//!
//! # What a frame costs
//!
//! A changed frame costs two dispatches and three uniform blobs; a held
//! frame costs neither. Everything that is a pure
//! function of the subject is measured once when it loads — the survey's
//! per-vertex classes and areas ([`survey`]), the bake's vertex buffer —
//! and everything that is a pure function of the seed and the canvas is
//! pulped once when the canvas is created: the paper's noise fields
//! ([`field::paper`], about thirty-two milliseconds at 900x1200, which is
//! twice a whole frame's budget) and the accident stream the uniform blob
//! replays ([`wash::SeedUniforms`]). What is left on a changed frame is the
//! two matrices, the chart's two dozen points per eye, one pass over the
//! vertices for the centroids, and the aperture geometry the chart moves.

pub mod accent;
#[cfg(test)]
mod crossfeed;
pub mod field;
pub mod image;
pub mod palette;
pub mod program;
pub mod regions;
pub mod survey;

use std::collections::VecDeque;

use aether_math::{Mat4, Vec2, Vec3};
use aether_render::{
    CreateGeometry, CreateTexture, DestroyTexture, DrawMaterialTextured, GeometrySlotSpec, MaterialRect,
    MaterialTexturedRect, ProgramDestroy, ProgramDispatch, ProgramRegister, QuadBlend, TextureFormat, TextureSampling,
    TextureUsage, UpdateGeometry,
};

use palette::Palette;
use program::sight::ToneUniforms;
use program::wash::{self, Canvas, Faces, Frame, Placement, Presence, SeedUniforms, WashBindings, WashProgram};
use program::{bake, face};
use survey::Survey;

use crate::anchor::Anchors;
use crate::chart::{self, Face};
use crate::deform::{BONE_LIMIT, Rigid, Skin};
use crate::extract::Settings;
use crate::labels;
use crate::mesh::Mesh;

/// Long-edge ceiling on the wash canvas.
///
/// The sheet develops at the window's own pixels up to this ceiling —
/// which is the resolution every distance in the engine was tuned at — and
/// the wash body under it develops coarser again by
/// [`wash::BODY_DIVISOR`]. The accents do not: an iris is a couple of
/// dozen pixels across at this framing and its slit a fraction of one, so
/// they are exactly the content the sheet's own pixels exist for.
pub(crate) const CANVAS_LONG_EDGE: usize = 1280;

/// Why the active drawing cannot be served without crossing the canvas
/// ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanvasCapacityError {
    pub needed: usize,
    pub capacity: usize,
}

/// A canvas at `long_edge`, preserving the requested aspect ratio with the
/// same floor rounding as the established long-edge clamp.
fn canvas_at_long_edge(width: usize, height: usize, long_edge: usize) -> Canvas {
    if width >= height {
        let short = height as u64 * long_edge as u64 / width as u64;
        Canvas { width: long_edge, height: usize::try_from(short.max(1)).expect("a canvas short edge fits usize") }
    } else {
        let short = width as u64 * long_edge as u64 / height as u64;
        Canvas { width: usize::try_from(short.max(1)).expect("a canvas short edge fits usize"), height: long_edge }
    }
}

/// Resolve the one canvas both ink and wash use.
///
/// The requested pixels stand unchanged while they fit under the long-edge
/// ceiling and already carry `required_texels`. Otherwise the long edge is
/// promoted to the smallest integer size whose aspect-preserving extent can
/// carry the drawing. A drawing that still does not fit at the ceiling is
/// refused with its exact requirement and the largest capacity this aspect
/// can provide.
///
/// Orchestration resolves this once and hands the result to both layers. The wash reads
/// its ink coverage plane out of a texture the ink layer's own program
/// writes (iamacoffeepot/aether#4451), and a program binding's size is
/// checked against the extent it was declared at, so the two agree on that
/// one texture exactly while they agree on the canvas it is derived from.
/// Two clamps that happened to match at the shipped framing would not be
/// an agreement; the dispatch that disagreed would warn-drop whole, and
/// the frame would lose its paint or its ink rather than show a wrong one.
pub fn resolve_canvas(width: u32, height: u32, required_texels: usize) -> Result<Canvas, CanvasCapacityError> {
    let (width, height) = ((width as usize).max(1), (height as usize).max(1));
    let requested_long = width.max(height);
    let base_long = requested_long.min(CANVAS_LONG_EDGE);
    let base = canvas_at_long_edge(width, height, base_long);
    if base.width * base.height >= required_texels {
        return Ok(base);
    }
    let ceiling = canvas_at_long_edge(width, height, CANVAS_LONG_EDGE);
    let capacity = ceiling.width * ceiling.height;
    if capacity < required_texels {
        return Err(CanvasCapacityError { needed: required_texels, capacity });
    }

    let mut low = base_long + 1;
    let mut high = CANVAS_LONG_EDGE;
    while low < high {
        let middle = low + (high - low) / 2;
        let candidate = canvas_at_long_edge(width, height, middle);
        if candidate.width * candidate.height >= required_texels {
            high = middle;
        } else {
            low = middle + 1;
        }
    }

    Ok(canvas_at_long_edge(width, height, low))
}

/// The window-only answer used by the independent silhouette instrument.
#[must_use]
pub fn wash_canvas(width: u32, height: u32) -> Canvas {
    let (width, height) = ((width as usize).max(1), (height as usize).max(1));

    canvas_at_long_edge(width, height, width.max(height).min(CANVAS_LONG_EDGE))
}

/// The studio's one seed — `Sumire` in ASCII — so the same view develops
/// the same painting, today and tomorrow.
const SHEET_SEED: u64 = 0x5375_6d69_7265;

/// Margin behind the subject's bounding radius where the sheet stands, in
/// world units, so grazing geometry never touches its own backdrop.
const SHEET_STANDOFF: f32 = 0.25;

/// Everything the easel needs from the camera to develop and to stand the
/// sheet: the same eye and matrix the ink was drawn from, and the framing
/// the sheet must fill.
pub struct View {
    pub eye: Vec3,
    pub target: Vec3,
    pub view_proj: Mat4,
    pub aspect: f32,
    /// Vertical field of view in radians — the sheet spans the frustum
    /// cross-section exactly, so the wash registers with the ink pixel for
    /// pixel.
    pub field_of_view: f32,
}

/// The chart geometry the accents are painted on.
///
/// Borrowed rather than owned, and handed in per develop rather than kept:
/// the easel consumes the chart, it does not hold a puppet. `mesh` is the
/// subject the ink plants its own marks against — the fine one, whatever
/// coarser stand-in the wash is rasterized from — because paint and ink
/// have to come to rest on the same fitted plane.
pub struct Chart<'a> {
    pub mesh: &'a Mesh,
    pub anchors: &'a Anchors,
    pub face: Face,
    /// Where the head bone sends a point at this pose, and where the eye
    /// stands in the head's own frame.
    ///
    /// The face is authored in the model's frontal plane and planted
    /// along it, so it is planted on the rest sculpt above and carried
    /// through this afterwards — and the aperture's occlusion is asked
    /// of the rest sculpt from an eye brought back through the same map,
    /// which is the same question at a rigidly turned head and the only
    /// one a mesh with no ray accelerator can answer.
    pub head: Rigid,
    pub eye: Vec3,
}

/// Everything the easel reads of the subject for one development: the
/// surface the wash bakes off, its per-vertex material scores
/// ([`labels::Labels::vertex_scores`], solved once at load), the drawing
/// solved for this eye, and the chart when the subject has a face.
/// Borrowed for the call — the easel keeps none of it.
pub struct Subject<'a> {
    /// The *rest* sculpt: what the bake's vertex buffer is packed from,
    /// and what the survey's per-vertex classes and areas are measured
    /// on. Both are uploaded or solved once per subject, and the pose
    /// reaches the bake through [`Subject::bones`] instead.
    pub mesh: &'a Mesh,
    /// The same sculpt skinned to this frame's pose, when one is
    /// running.
    ///
    /// What the develop's per-frame projections read: where each
    /// material's centroid lands on the page, which is where the wash
    /// places its stains. Those are a projection of vertices rather than
    /// a buffer, so they follow the pose here for what a pass over the
    /// vertices already cost.
    pub posed: Option<&'a Mesh>,
    pub scores: &'a [[f32; labels::CLASSES]],
    /// The box this subject is painted out of — its own authored one, or
    /// the built-in canonical box when it named none. Everything the
    /// develop reads of a class goes through it: the fall-through, which
    /// entries exist, and which classes the face machinery has to work
    /// over.
    pub palette: &'a Palette,
    pub settings: &'a Settings,
    /// The ink coverage plane for this eye: where the drawing landed,
    /// which the flow solve reads to find which way the hair runs.
    ///
    /// A texture id rather than triangles. The frame's own ink is
    /// rasterized from the visibility field (ADR-0172), and since
    /// iamacoffeepot/aether#4451 the plane is a reduction of that same
    /// raster — so the wash yields boundary duty to the ink actually
    /// drawn, and no caller splits the drawing on the CPU to hand it
    /// over. `None` before the ink layer's textures stand, which holds
    /// the develop rather than painting from a plane that is not there.
    pub ink: Option<u32>,
    pub chart: Option<Chart<'a>>,
    /// The rig binding the mesh, when the subject carries one. Its
    /// per-vertex influences ride the bake's vertex buffer, uploaded
    /// once with everything else there.
    pub skin: Option<&'a Skin>,
    /// This frame's pose, as [`deform::bone_uniform`] lays it out — the
    /// same table the ink's dispatches carry.
    ///
    /// The wash bakes its subject plane once per subject and had no
    /// update path for a buffer that moved, so while a pose ran it stood
    /// on the rest mesh and read as a ghost behind the posed ink. There
    /// is still no update path, and there does not have to be: the bake's
    /// vertex stage poses the subject from here, so the once-uploaded
    /// geometry stays correct and both layers pose from one source of
    /// truth (iamacoffeepot/aether#4462).
    ///
    /// [`deform::bone_uniform`]: crate::deform::bone_uniform
    pub bones: [f32; BONE_LIMIT * 12],
}

/// The two programs the develop registers, in the order it sends them —
/// which is the order the render cap answers them.
const BAKE: usize = 0;
const WASH: usize = 1;
const PROGRAMS: usize = 2;

/// The geometry slots the develop keeps resident, in create order.
const SUBJECT_GEOMETRY: usize = 0;
const APERTURE_GEOMETRY: usize = 1;
const GEOMETRIES: usize = 2;

/// Each slot beside the vertex layout its pass binds, in create order —
/// which is the order the ids answer in.
const GEOMETRY_SLOTS: [(usize, fn() -> GeometrySlotSpec); GEOMETRIES] =
    [(SUBJECT_GEOMETRY, bake::geometry_slot::<{ labels::CLASSES }>), (APERTURE_GEOMETRY, face::geometry_slot)];

/// The sampled plane textures one canvas carries, before the writable
/// sheet that completes the set.
const PLANE_COUNT: usize = 6;

/// One geometry's packed buffers, waiting for the mail that ships them.
/// Present means the bytes still have to travel; taken means the GPU
/// holds them.
struct GeometryBytes {
    vertices: Vec<u8>,
    indices: Vec<u8>,
}

/// The two uniform blobs one develop produced.
///
/// Kept rather than moved out with the dispatch: a develop at a view
/// already derived for re-stages these rather than re-deriving them, and
/// the dispatch needs its own copy either way.
struct Uniforms {
    bake: Vec<u8>,
    wash: Vec<u8>,
}

/// What one develop is a pure function of, as bit patterns so two frames
/// of one held view compare equal exactly.
///
/// Everything else it reads — the subject's mesh, its material scores,
/// the chart's anchors, the settings — is fixed for as long as the
/// subject stands, so it rides [`Easel::subject_changed`] rather than
/// this key. Anything added to the develop that varies within one subject
/// at one view has to join it here, or a held frame will keep serving the
/// answer before it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DevelopKey {
    eye: [u32; 3],
    view_proj: [u32; 16],
    canvas: Canvas,
    /// The pose the bake blob was encoded at.
    ///
    /// The subject's geometry no longer moves with the pose — the bake's
    /// vertex stage poses it from this very table — so the pose enters
    /// the develop through the uniform alone, and a key without it would
    /// serve the blob from the frame before and leave the wash standing
    /// on a pose the ink has left (iamacoffeepot/aether#4462).
    bones: [u32; BONE_LIMIT * 12],
}

impl DevelopKey {
    fn of(subject: &Subject<'_>, view: &View, canvas: Canvas) -> Self {
        Self {
            eye: view.eye.to_array().map(f32::to_bits),
            view_proj: view.view_proj.to_cols_array().map(f32::to_bits),
            canvas,
            bones: subject.bones.map(f32::to_bits),
        }
    }
}

/// Where a resident buffer stands: nothing sent yet, a create in flight
/// whose id has not come back, or the id the render cap assigned. Only
/// the last can be dispatched against, and only the first may send a
/// create.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Resident {
    #[default]
    Absent,
    Creating,
    Live(u32),
}

impl Resident {
    fn id(self) -> Option<u32> {
        match self {
            Self::Live(geometry_id) => Some(geometry_id),
            _ => None,
        }
    }
}

/// What one unanswered register was sent for. Ids come back in send
/// order, so the easel keeps one of these per register in flight and
/// matches them off the front.
///
/// A resize can re-lay the wash graph while its register is still
/// unanswered, and the id coming back then belongs to a program the
/// easel has already moved past — hence the third arm, which routes
/// that id to the release list instead of letting it become the one the
/// easel dispatches against.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Registering {
    /// The bake graph, which no canvas ever outgrows.
    Bake,
    /// The wash graph the easel means to dispatch against.
    Wash,
    /// A wash graph the canvas outgrew before its id arrived.
    Stale,
}

/// The creates for one canvas size are in flight; the render cap
/// answers them in send order, so the collected ids land in binding
/// order.
struct Creating {
    ids: Vec<u32>,
    canvas: Canvas,
}

/// The wash layer's state machine: the registered programs, the resident
/// geometry and textures, and this frame's staged develop.
#[derive(Default)]
pub struct Easel {
    canvas: Option<Canvas>,
    /// The wash program's graph and window layout, laid at the first
    /// develop and re-laid only when the canvas changes height — the one
    /// thing its blur extents are chosen against.
    program: Option<WashProgram>,
    /// The program ids the render cap assigned, by slot: [`BAKE`] then
    /// [`WASH`]. A slot is empty until its register answers, and the wash
    /// slot empties again whenever a resize re-lays the graph.
    programs: [Option<u32>; PROGRAMS],
    /// The registers in flight, in send order; hold further registers
    /// until every one of them answers.
    registering: VecDeque<Registering>,
    /// Programs a re-lay left behind, released once the canvas' own graph
    /// has been registered in their place.
    stale_programs: Vec<u32>,
    /// The bound registry textures and the canvas they were created at.
    textures: Option<(WashBindings, Canvas)>,
    /// Creates are in flight; hold further creates until they answer.
    creating: Option<Creating>,
    /// The three resident geometries, indexed by the slot constants.
    geometries: [Resident; GEOMETRIES],
    /// The bytes each geometry slot still owes the GPU, indexed the same
    /// way. A slot is `Some` from the develop that packed it until the
    /// create or the update that ships it, and `None` while the buffer up
    /// there is already the answer — which is every frame of a held view.
    packed: [Option<GeometryBytes>; GEOMETRIES],
    /// What the subject measures independent of the view, for the
    /// centroids the accidents are placed about.
    survey: Option<Survey>,
    /// The accident stream, rolled for this seed, canvas and visible set.
    seed_slice: Option<SeedUniforms>,
    /// The view the staged uniforms and packed geometries were derived
    /// for, so a develop at the same one re-derives nothing.
    derived_for: Option<DevelopKey>,
    /// The ink coverage plane the last develop was handed, re-read every
    /// frame rather than kept with the textures: the ink layer releases
    /// and re-creates its own set on a resize, so the id outlives no
    /// canvas change.
    ink_plane: Option<u32>,
    /// The version of the inputs the resident sheet ought to contain.
    ///
    /// A plain [`DevelopKey`] is not enough here: subject and chart
    /// invalidations can change the wash while eventually producing the
    /// same eye, matrix, canvas, bone table, and ink texture id. The ink
    /// layer also rewrites its coverage plane in place. Those paths clear
    /// `derived_for`, so committing their next derivation advances this
    /// revision even when its key compares equal to the one before it.
    revision: u64,
    /// The revision whose bake and wash dispatches were both emitted.
    /// Kept separate from `revision` so a gate that is not ready retries
    /// on the next frame rather than blessing a sheet it never wrote.
    dispatched: Option<u64>,
    /// The uniform blobs that key produced.
    uniforms: Option<Uniforms>,
    /// A develop is staged for this canvas, waiting for the mail. Cleared
    /// by the dispatch that ships it — a develop the render cap could not
    /// serve is dropped rather than queued, since the next frame's answer
    /// supersedes it anyway.
    staged: Option<Canvas>,
    /// At least one dispatch has been sent, so the sheet holds paint
    /// rather than the writable texture's transparent clear.
    developed: bool,
    /// The render cap refused a register or a create — the headless
    /// chassis' fail-fast reply — so the easel stops asking rather than
    /// warn-storming.
    disabled: bool,
}

/// One `f32` plane as the little-endian bytes an `R32Float` texture
/// stages.
fn plane_bytes(plane: &[f32]) -> Vec<u8> {
    plane.iter().flat_map(|value| value.to_le_bytes()).collect()
}

impl Easel {
    /// Name the gate that just held a develop back, with the state it
    /// read, at `debug` (iamacoffeepot/aether#4465).
    ///
    /// Every mail the develop ships is gated on the replies to the mail
    /// before it, and a gate that never opens produces no error and no
    /// warning anywhere — the layer keeps developing and simply never
    /// dispatches, which is a blank sheet and nothing to read. So each
    /// early return says which one it was and what it saw. Off at the
    /// default `info`; ask for it with `AETHER_LOG_FILTER=aether_puppet=debug`,
    /// or the `[runtime] log_filter` line of a `--config` file, which is
    /// the route a hub-forked substrate has.
    fn refused(&self, gate: &str, reason: &str) {
        tracing::debug!(
            target: "aether_puppet",
            gate,
            reason,
            programs = ?self.programs,
            registering = self.registering.len(),
            textures = self.textures.is_some(),
            creating = self.creating.is_some(),
            geometries = ?self.geometries.map(Resident::id),
            packed = ?self.packed.each_ref().map(Option::is_some),
            staged = ?self.staged,
            ink_plane = ?self.ink_plane,
            "easel gate refused",
        );
    }

    /// A canvas change orphans every plane pulped for the old one — the
    /// paper's grain is sampled at the canvas' own rate — so it releases
    /// the whole texture set and the seed slice rolled against it. The
    /// programs and the resident geometry survive: neither knows the
    /// canvas size.
    pub fn resized(&mut self, canvas: Canvas) {
        if self.canvas == Some(canvas) {
            return;
        }
        self.canvas = Some(canvas);
        self.seed_slice = None;
        self.derived_for = None;
    }

    /// A new subject or field arrived; everything measured off the old one
    /// has to be measured again.
    ///
    /// The wash graph is not among them. Its structure follows the
    /// painter's box rather than the subject, and most subjects arrive
    /// with the same box — so the next develop re-lays it only if the box
    /// under it actually changed, and a reload of the same box costs no
    /// re-register.
    pub fn subject_changed(&mut self) {
        self.survey = None;
        self.packed[SUBJECT_GEOMETRY] = None;
        self.geometries[SUBJECT_GEOMETRY] = Resident::Absent;
        self.derived_for = None;
    }

    /// Drop the wash graph and retire whatever program stands for it, so
    /// the next develop lays one for what has changed underneath.
    fn relay_wash(&mut self) {
        self.program = None;
        self.stale_programs.extend(self.programs[WASH].take());
        for sent_for in &mut self.registering {
            if *sent_for == Registering::Wash {
                *sent_for = Registering::Stale;
            }
        }
    }

    /// The chart changed while the subject and its resident geometry did not.
    ///
    /// Expression, gaze, viseme, and eye-style mail move only authored face
    /// marks. The next develop must re-project the eye frames and aperture,
    /// but the survey and subject buffers remain valid.
    pub fn chart_changed(&mut self) {
        self.derived_for = None;
    }

    /// The reply to one requested register. Ids arrive in send order, so
    /// each answers the front of the in-flight queue. `Err` disables the
    /// easel for the session, exactly as a refused create does; an id
    /// answering a register the canvas has outgrown joins the release
    /// list instead of becoming the one the easel dispatches against.
    pub fn registered(&mut self, result: Result<u32, ()>) {
        let sent_for = self.registering.pop_front();
        match (result, sent_for) {
            (Ok(program_id), Some(Registering::Bake)) => self.programs[BAKE] = Some(program_id),
            (Ok(program_id), Some(Registering::Wash)) => self.programs[WASH] = Some(program_id),
            (Ok(program_id), Some(Registering::Stale)) => self.stale_programs.push(program_id),
            (Ok(_), None) => {}
            (Err(()), _) => self.disabled = true,
        }
    }

    /// The reply to one requested create. Ids arrive in send order —
    /// binding order — and the seventh completes the set.
    pub fn created(&mut self, result: Result<u32, ()>) {
        let Ok(texture_id) = result else {
            self.creating = None;
            self.disabled = true;
            return;
        };
        let Some(mut creating) = self.creating.take() else {
            return;
        };

        creating.ids.push(texture_id);
        if creating.ids.len() < PLANE_COUNT + 1 {
            self.creating = Some(creating);
            return;
        }
        let Creating { ids, canvas } = creating;
        let bindings = WashBindings {
            packed: ids[0],
            tooth: ids[1],
            edge: ids[2],
            tooth_fine: ids[3],
            edge_fine: ids[4],
            paper_shade: ids[5],
            sheet: ids[6],
        };
        self.textures = Some((bindings, canvas));
    }

    /// The reply to one requested geometry create. Ids arrive in send
    /// order — the subject's, then the chart's. `Err` disables
    /// the easel for the session, as a refused plane create does: every
    /// geometry here is bound by a declared pass, so a program without one
    /// develops nothing worth showing.
    pub fn geometry_created(&mut self, result: Result<u32, ()>) {
        let Ok(geometry_id) = result else {
            self.disabled = true;
            return;
        };
        let Some(slot) = self.geometries.iter().position(|at| *at == Resident::Creating) else {
            return;
        };
        self.geometries[slot] = Resident::Live(geometry_id);
    }

    /// The window's own pixels, clamped to the long-edge ceiling.
    fn canvas(&self) -> Option<Canvas> {
        self.canvas
    }

    /// The exact planes a develop at the current view would paint from,
    /// for the dump diagnostic — same canvas clamp, same rasterize, no
    /// paint. `None` before the first resize. The CPU rasterize is the
    /// oracle's, not the frame path's.
    pub fn bake_planes(&self, subject: &Subject<'_>, view: &View) -> Option<regions::RegionPlanes> {
        let canvas = self.canvas()?;
        let (width, height) = canvas.body();

        Some(regions::rasterize(
            subject.mesh,
            subject.scores,
            subject.settings,
            view.eye,
            &view.view_proj,
            width,
            height,
        ))
    }

    /// Develop the sheet for this view.
    ///
    /// On every changed frame: measure the subject if it has not been
    /// measured, roll the accident stream if the visible set has changed,
    /// and stage the two uniform blobs and the two geometries that move.
    ///
    /// A held view is the exception, and it is most of what the frame
    /// used to cost (iamacoffeepot/aether#4447). Every quantity below is
    /// a pure function of the eye, the matrix and the canvas over a
    /// subject that has not changed, so a develop at a view already
    /// derived for re-derives nothing: no centroid pass over the vertices,
    /// no re-pack, no geometry mail for buffers already holding those very
    /// bytes, and no wash dispatch when that revision already stands in
    /// the sheet.
    ///
    /// The ink coverage plane is read out of `subject` on every develop,
    /// held view or not: it is an id the ink layer re-issues whenever its
    /// own textures are re-created, and a develop the key skipped still
    /// has to dispatch against the one standing now.
    pub fn develop(&mut self, subject: &Subject<'_>, view: &View) {
        if self.disabled {
            return;
        }
        let Some(canvas) = self.canvas() else {
            return;
        };
        let ink_changed = self.ink_plane != subject.ink;
        self.ink_plane = subject.ink;

        let key = DevelopKey::of(subject, view, canvas);
        if self.derived_for == Some(key) && self.uniforms.is_some() {
            if ink_changed {
                self.revision += 1;
                self.staged = Some(canvas);
            } else if self.dispatched != Some(self.revision) {
                self.staged = Some(canvas);
            }
            return;
        }

        let (body_width, body_height) = canvas.body();
        let Subject { mesh, posed, scores, settings, chart, skin, .. } = subject;
        // The rest sculpt is what a buffer or a per-subject measurement
        // is taken on; the posed copy is what a per-frame projection
        // reads.
        let surface = posed.unwrap_or(mesh);

        let survey = self.survey.get_or_insert_with(|| Survey::measure(mesh, scores, subject.palette));
        if self.packed[SUBJECT_GEOMETRY].is_none() && self.geometries[SUBJECT_GEOMETRY] == Resident::Absent {
            self.packed[SUBJECT_GEOMETRY] = Some(GeometryBytes {
                vertices: bake::vertices::<{ labels::CLASSES }>(mesh, scores, settings, *skin),
                indices: bake::indices(mesh),
            });
        }

        // Where each material sits, off the surface that projects into it
        // rather than off the pixels it covered — the class plane lives on
        // the GPU and ADR-0170 declines a readback (see `survey`).
        let centroids = survey.centroids(surface, view.eye, &view.view_proj, body_width, body_height);

        // The chart, asked afresh per frame rather than cached with the
        // anchors: gaze moves the iris inside its own aperture, so a frame
        // solved once at load would leave the paint looking where she used
        // to look.
        //
        // Planted on the rest sculpt and carried through the head's map
        // afterwards, which is how the ink's own marks reach the same
        // pose — an eye planted on a head that has turned is planted at a
        // graze.
        let rested: Vec<chart::EyeFrame> = chart
            .as_ref()
            .map(|chart| chart::eye_frames(chart.mesh, chart.anchors, chart.face, settings.eye_style))
            .unwrap_or_default();
        let head = chart.as_ref().map_or(Rigid::IDENTITY, |chart| chart.head);
        let frames: Vec<chart::EyeFrame> = rested.iter().cloned().map(|frame| frame.posed(&head)).collect();
        let fine_eyes = accent::project(&frames, &view.view_proj, canvas.width, canvas.height);
        let body_eyes = accent::project(&frames, &view.view_proj, body_width, body_height);
        // Asked of the rest sculpt from an eye brought back through the
        // head's map, because a posed copy carries no ray accelerator and
        // the two questions are the same one at a rigidly turned head.
        let charting = chart.as_ref().map_or(view.eye, |chart| chart.eye);
        let presence: Vec<f32> =
            fine_eyes.iter().map(|eye| survey::presence(mesh, charting, &rested[eye.frame()].aperture)).collect();

        let stains = stain_centres(subject.palette, &centroids, body_height);
        let placement = Placement { centroids: &centroids, stains: &stains, iris: iris_centre(&fine_eyes) };
        let wanted = Presence::of(&placement, subject.palette);

        // A canvas of a new height wants its own graph, and so does a new
        // painter's box: how far the water reaches in pixels decides the
        // extent each blur sweeps at, and the box decides how many chains
        // there are and which of them smear, glaze or throw a stain. The
        // bake graph knows neither and survives the re-lay.
        if self
            .program
            .as_ref()
            .is_none_or(|program| program.canvas_height() != canvas.height || program.palette() != subject.palette)
        {
            self.relay_wash();
        }
        let program = self.program.get_or_insert_with(|| wash::program(canvas.height, subject.palette));
        let slice = self
            .seed_slice
            .take()
            .filter(|slice| slice.serves(SHEET_SEED, canvas, wanted))
            .unwrap_or_else(|| program.seed_uniforms(SHEET_SEED, canvas, wanted));

        let faces = chart.as_ref().map(|_| Faces { fine: &fine_eyes, body: &body_eyes, presence: &presence });
        // Where the hand's attention is. The authored arm is a point in
        // the subject's own space, so it is projected here, at the body's
        // extent — which is where the care chain's own planes stand.
        let care = field::CareSource::resolve(subject.palette, |at| {
            regions::on_canvas(&view.view_proj, at, body_width, body_height)
        });
        let frame = Frame { placement, faces, care };

        self.uniforms = Some(Uniforms {
            bake: bake::BakeUniforms {
                view_proj: view.view_proj,
                eye: view.eye,
                bones: subject.bones,
                tone: ToneUniforms::of(settings, skin.is_some()),
                posed: skin.is_some(),
            }
            .encode(),
            wash: program.frame_uniforms(&slice, &frame),
        });
        self.packed[APERTURE_GEOMETRY] = Some(GeometryBytes {
            vertices: face::vertices(&fine_eyes, canvas.width, canvas.height),
            indices: face::indices(&fine_eyes, canvas.width, canvas.height),
        });
        self.seed_slice = Some(slice);
        self.derived_for = Some(key);
        self.revision += 1;
        self.staged = Some(canvas);
    }

    /// The programs a re-lay left behind, to release after the canvas'
    /// own graph has registered — sent then rather than at the re-lay so
    /// nothing is destroyed while a dispatch could still name it.
    pub fn take_program_destroys(&mut self) -> Vec<ProgramDestroy> {
        if self.programs[WASH].is_none() {
            return Vec::new();
        }
        self.stale_programs.drain(..).map(|program_id| ProgramDestroy { program_id }).collect()
    }

    /// The register mail carrying whichever programs the easel is missing:
    /// both at the first develop — by then the GPU device exists, since a
    /// develop only ever follows a render stage — and the wash's alone
    /// whenever a resize re-lays its graph. The replies land in
    /// [`Easel::registered`] in this order.
    pub fn take_registers(&mut self) -> Vec<ProgramRegister> {
        if self.disabled || !self.registering.is_empty() || self.staged.is_none() {
            self.refused(
                "registers",
                if self.disabled {
                    "disabled"
                } else if !self.registering.is_empty() {
                    "registers in flight"
                } else {
                    "nothing staged"
                },
            );
            return Vec::new();
        }
        let Some(program) = self.program.as_ref() else {
            self.refused("registers", "no wash graph laid");
            return Vec::new();
        };

        let mut registers = Vec::with_capacity(PROGRAMS);
        if self.programs[BAKE].is_none() {
            self.registering.push_back(Registering::Bake);
            registers.push(bake::program::<{ labels::CLASSES }>());
        }
        if self.programs[WASH].is_none() {
            self.registering.push_back(Registering::Wash);
            registers.push(program.register().clone());
        }
        registers
    }

    /// The textures whose canvas no longer matches the staged develop, to
    /// release before the next creates. Resize is the only path here, and
    /// it releases the whole set — the plane textures and the sheet.
    pub fn take_destroys(&mut self) -> Vec<DestroyTexture> {
        if self.creating.is_some() {
            return Vec::new();
        }
        let stale = self.textures.as_ref().zip(self.staged).is_some_and(|((_, created), staged)| *created != staged);
        if !stale {
            return Vec::new();
        }

        let Some((bindings, _)) = self.textures.take() else {
            return Vec::new();
        };
        self.dispatched = None;
        bindings.owned().into_iter().map(|texture_id| DestroyTexture { texture_id }).collect()
    }

    /// The creates carrying this canvas' resident planes: the paper pulped
    /// at both extents, the packed plane the bake writes, and the writable
    /// sheet the composite resolves into.
    ///
    /// This is where the thirty-two milliseconds of noise go — once per
    /// canvas, never again, because none of it can turn with the view.
    pub fn take_creates(&mut self) -> Vec<CreateTexture> {
        if self.disabled
            || self.programs.iter().any(Option::is_none)
            || self.creating.is_some()
            || self.textures.is_some()
        {
            self.refused(
                "creates",
                if self.disabled {
                    "disabled"
                } else if self.programs.iter().any(Option::is_none) {
                    "programs unregistered"
                } else if self.creating.is_some() {
                    "creates in flight"
                } else {
                    "textures stand"
                },
            );
            return Vec::new();
        }
        let Some(canvas) = self.staged else {
            self.refused("creates", "nothing staged");
            return Vec::new();
        };
        let (body_width, body_height) = canvas.body();

        let body = field::paper(SHEET_SEED, body_width, body_height);
        let fine = field::paper(SHEET_SEED, canvas.width, canvas.height);
        let data = |width: usize, height: usize, plane: &[f32]| CreateTexture {
            width: width as u32,
            height: height as u32,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: plane_bytes(plane),
        };
        let target = |width: usize, height: usize, sampling: TextureSampling| CreateTexture {
            width: width as u32,
            height: height as u32,
            format: TextureFormat::Rgba8,
            sampling,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        };

        self.creating = Some(Creating { ids: Vec::new(), canvas });
        vec![
            // The bake's own output. Nearest is not a preference here: the
            // class channel is an integer in disguise, and a linear filter
            // across a material boundary averages the labels either side
            // into a third the surface never carried (`program::bake`).
            target(body_width, body_height, TextureSampling::Nearest),
            data(body_width, body_height, &body.noise.tooth),
            data(body_width, body_height, &body.noise.edge),
            data(canvas.width, canvas.height, &fine.noise.tooth),
            data(canvas.width, canvas.height, &fine.noise.edge),
            data(canvas.width, canvas.height, &fine.shade),
            target(canvas.width, canvas.height, TextureSampling::Linear),
        ]
    }

    /// The geometry creates, in the order [`Easel::geometry_created`]
    /// collects their ids: the subject the bake rasterizes, then the
    /// chart's aperture loops the face pass fills. One create each, then
    /// updated in place for the session.
    ///
    /// The bytes are moved out of the packed slot rather than copied out
    /// of it. What the slot then says is the truth the frame wants: this
    /// buffer is the GPU's, and nothing owes it an upload.
    pub fn take_geometry_creates(&mut self) -> Vec<CreateGeometry> {
        if self.disabled || self.staged.is_none() {
            self.refused(
                "geometry creates",
                if self.disabled {
                    "disabled"
                } else {
                    "nothing staged"
                },
            );
            return Vec::new();
        }
        // One create in flight at a time: the reply carries no slot, so
        // the collector matches it against the one slot that is asking.
        if self.geometries.contains(&Resident::Creating) {
            self.refused("geometry creates", "a create in flight");
            return Vec::new();
        }

        for (slot, geometry_slot) in GEOMETRY_SLOTS {
            if self.geometries[slot] != Resident::Absent {
                continue;
            }
            let Some(bytes) = self.packed[slot].take() else {
                self.refused("geometry creates", "an absent slot has nothing packed to ship");
                continue;
            };

            self.geometries[slot] = Resident::Creating;
            return vec![CreateGeometry {
                layout: geometry_slot().layout,
                vertices: bytes.vertices,
                indices: bytes.indices,
            }];
        }

        Vec::new()
    }

    /// The one geometry that moves with the eye, replacing the resident
    /// bytes in place. The subject's does not: nothing in its buffer turns
    /// with the view, so it is uploaded once and left alone.
    ///
    /// A slot only ships when it owes bytes *and* has an id to ship them
    /// against — so a develop that re-derived nothing sends no mail here
    /// at all (iamacoffeepot/aether#4447).
    pub fn take_geometry_updates(&mut self) -> Vec<UpdateGeometry> {
        let mut updates = Vec::new();
        for slot in [APERTURE_GEOMETRY] {
            let Some(geometry_id) = self.geometries[slot].id() else {
                continue;
            };
            let Some(bytes) = self.packed[slot].take() else {
                continue;
            };

            updates.push(UpdateGeometry { geometry_id, vertices: bytes.vertices, indices: bytes.indices });
        }

        updates
    }

    /// The two dispatches developing this frame: the bake filling the
    /// packed plane off the subject's own geometry, then the wash reading
    /// it. Sent after the geometry updates they read, in the same frame,
    /// and in this order — the wash samples what the bake wrote.
    pub fn take_dispatch(&mut self) -> Vec<ProgramDispatch> {
        if self.dispatched == Some(self.revision) {
            return Vec::new();
        }
        let (Some(bake_id), Some(wash_id)) = (self.programs[BAKE], self.programs[WASH]) else {
            self.refused("dispatch", "programs unregistered");
            return Vec::new();
        };
        // The wash reads where the ink stands, and the ink layer owns
        // that texture. Before its own set is created there is nothing to
        // bind, and a dispatch short one binding is dropped whole — so
        // the develop waits rather than being dropped in the cap.
        let Some(ink) = self.ink_plane else {
            self.refused("dispatch", "no ink coverage plane");
            return Vec::new();
        };
        let Some((bindings, canvas)) = self.textures.as_ref() else {
            self.refused("dispatch", "no texture set");
            return Vec::new();
        };
        let live: Option<Vec<u32>> = self.geometries.iter().map(|at| at.id()).collect();
        let (Some(ids), Some(staged)) = (live, self.staged.take()) else {
            self.refused("dispatch", "a geometry slot is not live");
            return Vec::new();
        };
        let Some(uniforms) = self.uniforms.as_ref() else {
            self.refused("dispatch", "no uniforms derived");
            return Vec::new();
        };
        if staged != *canvas {
            self.refused("dispatch", "the staged canvas is not the textures'");
            return Vec::new();
        }

        if !self.developed {
            tracing::debug!(target: "aether_puppet", ?staged, "the easel reached its first dispatch");
        }
        self.developed = true;
        self.dispatched = Some(self.revision);
        vec![
            ProgramDispatch {
                program_id: bake_id,
                bindings: vec![bindings.packed],
                geometries: vec![ids[SUBJECT_GEOMETRY]],
                uniforms: uniforms.bake.clone(),
            },
            ProgramDispatch {
                program_id: wash_id,
                bindings: bindings.dispatched(ink),
                geometries: vec![ids[APERTURE_GEOMETRY]],
                uniforms: uniforms.wash.clone(),
            },
        ]
    }

    /// The sheet, standing behind the subject and facing the eye.
    ///
    /// It spans the view frustum's cross-section exactly at its depth, so
    /// the painting — developed through the same camera — projects back
    /// onto the window pixel for pixel and the wash lands in the ink's
    /// lines. Behind every ribbon by construction, the depth test keeps
    /// the drawing on top.
    pub fn draw(&self, view: &View, subject_radius: f32) -> Option<DrawMaterialTextured> {
        if !self.developed {
            return None;
        }
        let (bindings, _) = self.textures.as_ref()?;

        let forward = (view.target - view.eye).normalize_or(Vec3::new(0.0, 0.0, -1.0));
        let sheet_depth = (view.target - view.eye).length() + subject_radius + SHEET_STANDOFF;
        let centre = view.eye + forward * sheet_depth;
        let right = forward.cross(Vec3::new(0.0, 1.0, 0.0)).normalize_or(Vec3::new(1.0, 0.0, 0.0));
        let up = right.cross(forward);

        let half_height = sheet_depth * (view.field_of_view * 0.5).tan();
        let half_width = half_height * view.aspect;
        let origin = centre - right * half_width - up * half_height;

        Some(DrawMaterialTextured {
            texture_id: bindings.sheet,
            // The developed sheet is opaque, so the two blends agree on
            // it; it stays what it has always been.
            blend: QuadBlend::Straight,
            rects: vec![MaterialTexturedRect {
                rect: MaterialRect {
                    x: origin.x,
                    y: origin.y,
                    z: origin.z,
                    width: half_width * 2.0,
                    height: half_height * 2.0,
                    right: right.to_array(),
                    up: up.to_array(),
                },
                // The canvas' first row is the top of the view; the rect's
                // `v` runs up from its origin corner, so the vertical axis
                // flips here.
                u0: 0.0,
                v0: 1.0,
                u1: 1.0,
                v1: 0.0,
                tint: aether_math::Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        })
    }
}

/// Where each material's atmosphere stain sits, about which its pours,
/// its lost edge and its thrown drops are placed.
///
/// The stain *itself* is placed exactly: `fs_atmosphere_spill` reads the
/// material's halo and the standing figure off planes the GPU already
/// holds, so the mask the stain develops from is the oracle's own. What
/// this estimates is only the pole the accidents inside that mask are
/// measured about, and the estimate is the region's centre carried along
/// the material's own drift.
///
/// It is an approximation, and an honest account of it is that the spill's
/// true centroid sits further out again: the figure cuts the halo back
/// wherever it stands, so what survives hangs off the silhouette rather
/// than straddling the region. On the parity fixture the two sit some
/// twenty texels apart on a hundred-and-twenty-texel canvas. That
/// redistributes the stain's own internal texture — which of its three
/// pours lands where, which way its edge is given up — without moving the
/// stain, and the reference board records this mark as its weakest
/// anyway: displacing a blur is not the same as pouring deliberately.
/// Deciding where the air ought to go is a taste pass, and when it is
/// taken the pole is what it will name.
fn stain_centres(
    palette: &Palette,
    centroids: &[Option<Vec2>; survey::SLOTS],
    height: usize,
) -> [Option<Vec2>; survey::SLOTS] {
    let mut stains = [None; survey::SLOTS];
    for material in palette.materials() {
        let Some(policy) = material.atmosphere.as_ref() else {
            continue;
        };
        let drift = policy.carried(height);
        let Some(slot) = stains.get_mut(usize::from(material.class)) else {
            continue;
        };
        *slot = centroids.get(usize::from(material.class)).copied().flatten().map(|centre| centre + drift);
    }

    stains
}

/// Where the iris meta-material sits on the sheet.
///
/// The chart owns the iris the way the field owns every other region, so
/// this is measured off the projected eyes rather than off a class:
/// the mean of the iris centres weighted by how much canvas each ellipse
/// covers, which is the area-weighted mean the coverage plane's own
/// centroid would have given. An eye turned edge-on covers nothing and
/// weighs nothing, exactly as its collapsed coverage would.
fn iris_centre(eyes: &[accent::Eye]) -> Option<Vec2> {
    let (mut sum, mut weight) = (Vec2::new(0.0, 0.0), 0.0);
    for eye in eyes {
        let area = eye.size() * eye.size();
        sum += eye.centre() * area;
        weight += area;
    }

    (weight > 0.0).then(|| sum / weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::LazyLock;

    use crate::deform::bone_uniform;
    use crate::labels::CLASSES;
    use crate::mesh::Mesh;

    /// Every fixture here is her own classes, so the box is hers. Held
    /// once rather than built per literal, because a `Subject` borrows
    /// the box it is painted out of.
    static PALETTE: LazyLock<Palette> = LazyLock::new(Palette::canonical);

    /// A stand-in for the ink layer's coverage plane. Nothing here reads
    /// it — the render cap does — so any id the easel would not otherwise
    /// hand out serves.
    const INK: u32 = 9_000;

    #[test]
    fn the_canvas_keeps_requested_pixels_until_the_drawing_needs_promotion() {
        assert_eq!(resolve_canvas(512, 512, 512 * 512), Ok(Canvas { width: 512, height: 512 }));
        assert_eq!(resolve_canvas(512, 512, 660 * 660), Ok(Canvas { width: 660, height: 660 }));
        assert_eq!(resolve_canvas(664, 664, 660 * 660), Ok(Canvas { width: 664, height: 664 }));
    }

    #[test]
    fn promotion_preserves_aspect_and_reports_the_ceiling_capacity_exactly() {
        assert_eq!(resolve_canvas(320, 240, 640 * 480), Ok(Canvas { width: 640, height: 480 }));
        assert_eq!(
            resolve_canvas(320, 240, 1280 * 960 + 1),
            Err(CanvasCapacityError { needed: 1280 * 960 + 1, capacity: 1280 * 960 }),
        );
    }

    /// A quad standing well inside the frame, every vertex skin.
    fn quad() -> Mesh {
        Mesh::from_obj_bytes(b"v -0.5 -0.5 0\nv 0.5 -0.5 0\nv 0.5 0.5 0\nv -0.5 0.5 0\nf 1 2 3\nf 1 3 4\n", 0)
            .expect("fixture mesh")
    }

    fn view() -> View {
        view_from(Vec3::new(0.0, 0.0, 5.0))
    }

    fn view_from(eye: Vec3) -> View {
        View {
            eye,
            target: Vec3::ZERO,
            view_proj: Mat4::perspective_rh(1.0, 0.75, 0.1, 100.0) * Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y),
            aspect: 0.75,
            field_of_view: 1.0,
        }
    }

    /// Tripwire: the easel must actually reach its dispatches.
    ///
    /// Every mail the develop ships is gated on the replies to the mail
    /// before it — registers, then plane creates, then one geometry
    /// create at a time — and each gate is a condition on state some
    /// other method owns. A gate that can never open produces no error
    /// and no warning anywhere: the component keeps developing, keeps
    /// staging a frame, and simply never sends a dispatch, so the sheet
    /// is absent and the drawing renders over bare background. That is
    /// exactly what a mis-wired register queue or a create counter off by
    /// one looks like from outside, and nothing else in the suite would
    /// notice.
    ///
    /// So this drives the whole protocol against a stand-in render cap —
    /// ids handed back in send order, as the real one does — and asserts
    /// the easel arrives at both dispatches and stands its sheet.
    #[test]
    fn the_develop_reaches_its_dispatches_through_the_whole_reply_protocol() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();

        let mut easel = Easel::default();
        easel.resized(wash_canvas(160, 200));

        let mut next_id = 0u32;
        let mut ids = || {
            next_id += 1;
            next_id
        };
        // Ten render stages is far more than the protocol needs — the
        // point is that it converges, not how fast.
        for _ in 0..10 {
            let subject = Subject {
                mesh: &mesh,
                posed: None,
                scores: &scores,
                palette: &PALETTE,
                settings: &settings,
                ink: Some(INK),
                chart: None,
                skin: None,
                bones: bone_uniform(&[]),
            };
            easel.develop(&subject, &view());

            for _ in easel.take_program_destroys() {}
            for _ in easel.take_registers() {
                easel.registered(Ok(ids()));
            }
            for _ in easel.take_destroys() {}
            for _ in easel.take_creates() {
                easel.created(Ok(ids()));
            }
            for _ in easel.take_geometry_creates() {
                easel.geometry_created(Ok(ids()));
            }
            for _ in easel.take_geometry_updates() {}
            if easel.take_dispatch().len() == 2 {
                assert!(
                    easel.draw(&view(), 1.0).is_some(),
                    "a dispatched develop must stand its sheet for the frame to show",
                );
                return;
            }
        }

        panic!(
            "the easel never reached its dispatches: programs {:?}, textures {}, geometries {:?}",
            easel.programs,
            easel.textures.is_some(),
            easel.geometries.map(Resident::id),
        );
    }

    /// Drive one easel to convergence against a stand-in render cap, so a
    /// test past the reply protocol can ask what a steady frame does.
    fn converged(mesh: &Mesh, scores: &[[f32; CLASSES]], settings: &Settings, at: &View) -> Easel {
        let mut easel = Easel::default();
        easel.resized(wash_canvas(160, 200));

        let mut next_id = 0u32;
        let mut ids = || {
            next_id += 1;
            next_id
        };
        for _ in 0..10 {
            let subject = Subject {
                mesh,
                posed: None,
                scores,
                palette: &PALETTE,
                settings,
                ink: Some(INK),
                chart: None,
                skin: None,
                bones: bone_uniform(&[]),
            };
            easel.develop(&subject, at);

            for _ in easel.take_registers() {
                easel.registered(Ok(ids()));
            }
            for _ in easel.take_creates() {
                easel.created(Ok(ids()));
            }
            for _ in easel.take_geometry_creates() {
                easel.geometry_created(Ok(ids()));
            }
            for _ in easel.take_geometry_updates() {}
            if easel.take_dispatch().len() == 2 {
                return easel;
            }
        }

        panic!("the easel never converged, so nothing past the reply protocol can be asked of it");
    }

    /// Tripwire: a held view must ship no geometry or program mail, and a
    /// moved eye must ship both again.
    ///
    /// This is the whole of iamacoffeepot/aether#4447 as a claim about
    /// mail rather than about milliseconds, and both ways of getting it
    /// wrong are silent. A develop key that never matches renders exactly
    /// the same picture and quietly re-uploads six megabytes a frame — the
    /// saving evaporates with nothing to notice it. A key that always
    /// matches renders a *stale* picture: the wash keeps developing off
    /// the drawing solved for the eye it first saw, so an orbit turns the
    /// ink and leaves the paint behind, which reads as the wash being
    /// slightly wrong rather than as a fault.
    #[test]
    fn a_held_view_writes_nothing_and_a_moved_eye_writes_again() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();
        let subject = || Subject {
            mesh: &mesh,
            posed: None,
            scores: &scores,
            palette: &PALETTE,
            settings: &settings,
            ink: Some(INK),
            chart: None,
            skin: None,
            bones: bone_uniform(&[]),
        };
        let mut easel = converged(&mesh, &scores, &settings, &view());

        easel.develop(&subject(), &view());
        assert!(
            easel.take_geometry_updates().is_empty(),
            "a develop at the view already derived for owes the GPU nothing"
        );
        assert!(
            easel.take_dispatch().is_empty(),
            "the resident sheet is byte-identical because a held frame writes none of its planes",
        );
        assert!(easel.draw(&view(), 1.0).is_some(), "the held frame keeps presenting the resident sheet");

        easel.develop(&subject(), &view_from(Vec3::new(3.0, 1.0, 4.0)));
        assert_eq!(
            easel.take_geometry_updates().len(),
            1,
            "an eye that moved re-derives the aperture, and it has to travel",
        );
        assert_eq!(easel.take_dispatch().len(), 2, "the moved view bakes and washes its new answer");
    }

    /// A pose reaches both the bake and the in-place ink coverage through
    /// the bone table. It is therefore a wash input even when the camera
    /// itself is held.
    #[test]
    fn a_changed_bone_table_redevelops_at_a_held_view() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();
        let mut bones = bone_uniform(&[]);
        bones[3] = 0.25;
        let subject = Subject {
            mesh: &mesh,
            posed: None,
            scores: &scores,
            palette: &PALETTE,
            settings: &settings,
            ink: Some(INK),
            chart: None,
            skin: None,
            bones,
        };
        let mut easel = converged(&mesh, &scores, &settings, &view());

        easel.develop(&subject, &view());
        assert_eq!(easel.take_geometry_updates().len(), 1, "the pose re-derives the projected aperture");
        assert_eq!(easel.take_dispatch().len(), 2, "the held camera does not hide a changed pose");
    }

    /// Subject replacement can leave every bit in `DevelopKey` and the
    /// ink texture id unchanged. It still replaces the mesh and rewrites
    /// the coverage plane in place, so it must advance the dispatch
    /// revision rather than comparing equal to the old sheet.
    #[test]
    fn a_subject_invalidation_redevelops_even_when_the_view_key_is_equal() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();
        let subject = Subject {
            mesh: &mesh,
            posed: None,
            scores: &scores,
            palette: &PALETTE,
            settings: &settings,
            ink: Some(INK),
            chart: None,
            skin: None,
            bones: bone_uniform(&[]),
        };
        let mut easel = converged(&mesh, &scores, &settings, &view());

        easel.subject_changed();
        easel.develop(&subject, &view());
        assert_eq!(easel.take_geometry_creates().len(), 1, "the replacement subject gets a new resident buffer");
        easel.geometry_created(Ok(9_001));
        assert_eq!(easel.take_geometry_updates().len(), 1, "the aperture is re-derived with the subject");
        assert_eq!(easel.take_dispatch().len(), 2, "the equal view key must not preserve the old subject's wash");
    }

    #[test]
    fn a_chart_change_invalidates_only_the_chart_derivation() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();
        let mut easel = converged(&mesh, &scores, &settings, &view());
        let geometry = easel.geometries[SUBJECT_GEOMETRY].id();

        assert!(easel.derived_for.is_some());
        assert!(easel.survey.is_some());
        easel.chart_changed();

        assert!(easel.derived_for.is_none(), "the next develop must re-project the chart");
        assert!(easel.survey.is_some(), "chart state does not invalidate the material survey");
        assert_eq!(
            easel.geometries[SUBJECT_GEOMETRY].id(),
            geometry,
            "chart state must not discard resident subject geometry",
        );
    }

    /// A chart change rewrites both ink coverage and wash inputs while
    /// keeping the same texture id. Repeating the same invalidation must
    /// therefore dispatch each time, and a derivation from identical
    /// inputs must produce identical mail.
    #[test]
    fn a_chart_invalidation_redevelops_same_id_coverage_deterministically() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();
        let subject = || Subject {
            mesh: &mesh,
            posed: None,
            scores: &scores,
            palette: &PALETTE,
            settings: &settings,
            ink: Some(INK),
            chart: None,
            skin: None,
            bones: bone_uniform(&[]),
        };
        let mut easel = converged(&mesh, &scores, &settings, &view());

        let dispatches = (0..2)
            .map(|_| {
                easel.chart_changed();
                easel.develop(&subject(), &view());
                assert_eq!(easel.take_geometry_updates().len(), 1, "the aperture follows the chart invalidation");
                let dispatches = easel.take_dispatch();
                assert_eq!(dispatches.len(), 2, "same-id coverage is not mistaken for the coverage already painted");
                dispatches
            })
            .collect::<Vec<_>>();

        for (first, second) in dispatches[0].iter().zip(&dispatches[1]) {
            assert_eq!(first.program_id, second.program_id);
            assert_eq!(first.bindings, second.bindings);
            assert_eq!(first.geometries, second.geometries);
            assert_eq!(first.uniforms, second.uniforms);
        }
    }

    /// The ink layer re-creates its texture set on a resize. Even when
    /// every derived wash input is unchanged, the wash has to bind and
    /// read the replacement coverage plane exactly once.
    #[test]
    fn a_replaced_ink_plane_redevelops_once_against_the_new_id() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();
        let mut easel = converged(&mesh, &scores, &settings, &view());
        let replacement = INK + 1;
        let subject = || Subject {
            mesh: &mesh,
            posed: None,
            scores: &scores,
            palette: &PALETTE,
            settings: &settings,
            ink: Some(replacement),
            chart: None,
            skin: None,
            bones: bone_uniform(&[]),
        };

        easel.develop(&subject(), &view());
        let dispatches = easel.take_dispatch();
        assert_eq!(dispatches.len(), 2, "the replacement coverage plane invalidates the wash");
        assert!(dispatches[1].bindings.contains(&replacement), "the wash binds the replacement plane");

        easel.develop(&subject(), &view());
        assert!(easel.take_dispatch().is_empty(), "the same replacement id does not invalidate a second time");
    }

    // Tripwire: a canvas change must release the paper pulped for the old
    // one. The paper's grain is sampled at the canvas' own rate, so a
    // sheet kept across a resize granulates against a texture of the wrong
    // size — and the shipped bug this pins is its ancestor (issue 4391 —
    // the displaced ear flush after fullscreening).
    #[test]
    fn a_resize_orphans_the_seed_slice_and_a_same_size_announcement_keeps_it() {
        let mut easel = Easel::default();
        easel.resized(wash_canvas(800, 1000));
        let canvas = easel.canvas().expect("a resized easel has a canvas");
        easel.seed_slice =
            Some(wash::program(canvas.height, &PALETTE).seed_uniforms(SHEET_SEED, canvas, Presence::default()));

        easel.resized(wash_canvas(800, 1000));
        assert!(easel.seed_slice.is_some(), "a same-size announcement must not orphan the accident stream");

        easel.resized(wash_canvas(3024, 1670));
        assert!(easel.seed_slice.is_none(), "a canvas change must, so the next develop re-rolls at the new size");
    }

    #[test]
    fn a_resize_invalidates_the_dispatched_sheet_and_releases_its_textures() {
        let mesh = quad();
        let scores = vec![[1.0; CLASSES]; mesh.positions.len()];
        let settings = Settings::default();
        let subject = Subject {
            mesh: &mesh,
            posed: None,
            scores: &scores,
            palette: &PALETTE,
            settings: &settings,
            ink: Some(INK),
            chart: None,
            skin: None,
            bones: bone_uniform(&[]),
        };
        let mut easel = converged(&mesh, &scores, &settings, &view());
        let standing = easel.revision;

        easel.resized(wash_canvas(320, 400));
        easel.develop(&subject, &view());
        assert_ne!(easel.revision, standing, "the new canvas advances the wash revision");
        assert_eq!(easel.take_destroys().len(), 7, "the old canvas' whole texture set is released");
        assert!(easel.dispatched.is_none(), "fresh textures must be refilled before the revision can stand");
    }

    // A register answered after the canvas outgrew the graph it was sent
    // for names a program the easel must not dispatch against: its blur
    // extents were chosen for the old canvas, so every reduced chain
    // would sweep the wrong plane. The bug shape without this routing is
    // worse than a wrong develop — the stale id fills the wash slot, the
    // register gate sees a program already registered, and the graph for
    // the canvas actually on screen is never sent at all.
    #[test]
    fn a_register_answered_after_a_re_lay_is_released_rather_than_dispatched_against() {
        // The wash register the resize overtook, then the one it sent in
        // its place; ids come back in that order.
        let registering = VecDeque::from([Registering::Stale, Registering::Wash]);
        let mut easel = Easel { registering, ..Easel::default() };

        easel.registered(Ok(7));
        assert_eq!(easel.programs[WASH], None, "a stale register must not become the program the easel dispatches");
        assert!(easel.take_program_destroys().is_empty(), "nothing is released before the canvas' own graph is live");

        easel.registered(Ok(9));
        assert_eq!(easel.programs[WASH], Some(9), "the register after it answers for the canvas' own graph");
        assert_eq!(
            easel.take_program_destroys().into_iter().map(|destroy| destroy.program_id).collect::<Vec<u32>>(),
            vec![7],
            "the graph left behind is released once its replacement is live",
        );
    }
}
