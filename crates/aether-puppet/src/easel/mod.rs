//! The easel: watercolour laid under the drawing
//! (iamacoffeepot/aether#4349).
//!
//! The wash runs at frame rate. Each render stage develops the whole
//! painting afresh — a sheet of painted paper standing behind the subject,
//! re-oriented to face the eye — and costs one textured material rect to
//! show. The ink is solved live above it and wins the depth test, so the
//! drawing and the paint under it move together through an orbit rather
//! than the paint lagging a held view behind (iamacoffeepot/aether#4387).
//!
//! [`field`] turns a region into wet paint on the CPU — the parity
//! oracle — while [`program`] speaks the same develop as registered
//! ADR-0170/0171 render programs; [`palette`] owns the pigments;
//! [`regions`] keeps the CPU bake the oracle rasterizes through.
//! This module is the orchestrator: what is resident, what is staged per
//! frame, and where the sheet stands.
//!
//! # What a frame costs
//!
//! Two dispatches and three uniform blobs. Everything that is a pure
//! function of the subject is measured once when it loads — the survey's
//! per-vertex classes and areas ([`survey`]), the bake's vertex buffer —
//! and everything that is a pure function of the seed and the canvas is
//! pulped once when the canvas is created: the paper's noise fields
//! ([`field::paper`], about thirty-two milliseconds at 900x1200, which is
//! twice a whole frame's budget) and the accident stream the uniform blob
//! replays ([`wash::SeedUniforms`]). What is left per frame is the two
//! matrices, the chart's two dozen points per eye, one pass over the
//! vertices for the centroids, and the ribbon and aperture geometries the
//! drawing and the chart move.

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
    CreateGeometry, CreateTexture, DestroyTexture, DrawMaterialTextured, DrawTriangle, MaterialRect,
    MaterialTexturedRect, ProgramDestroy, ProgramDispatch, ProgramRegister, QuadBlend, TextureFormat, TextureSampling,
    TextureUsage, UpdateGeometry,
};

use program::wash::{self, Canvas, Faces, Frame, Placement, Presence, SeedUniforms, WashBindings, WashProgram};
use program::{bake, face, ink};
use survey::Survey;

use crate::anchor::Anchors;
use crate::chart::{self, Face};
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
}

/// Everything the easel reads of the subject for one development: the
/// surface the wash bakes off, its per-vertex material scores
/// ([`labels::Labels::vertex_scores`], solved once at load), the drawing
/// solved for this eye, and the chart when the subject has a face.
/// Borrowed for the call — the easel keeps none of it.
pub struct Subject<'a> {
    pub mesh: &'a Mesh,
    pub scores: &'a [[f32; labels::CLASSES]],
    pub settings: &'a Settings,
    /// The drawing solved for this eye, asked for rather than handed
    /// over: the frame's own ink is rasterized from the visibility field
    /// (ADR-0172) and no CPU triangles exist for it, so the easel is the
    /// only caller that still needs the visible runs — it rasterizes its
    /// ink coverage plane from them, and the flow solve reads that plane.
    /// A closure rather than a slice because a caller with no easel work
    /// to do never pays for the split.
    pub drawn: &'a dyn Fn() -> Vec<DrawTriangle>,
    pub chart: Option<Chart<'a>>,
}

/// The two programs the develop registers, in the order it sends them —
/// which is the order the render cap answers them.
const BAKE: usize = 0;
const WASH: usize = 1;
const PROGRAMS: usize = 2;

/// The geometry slots the develop keeps resident, in create order.
const SUBJECT_GEOMETRY: usize = 0;
const INK_GEOMETRY: usize = 1;
const APERTURE_GEOMETRY: usize = 2;
const GEOMETRIES: usize = 3;

/// The sampled plane textures one canvas carries, before the writable
/// sheet that completes the set.
const PLANE_COUNT: usize = 6;

/// One geometry's packed buffers, waiting for the mail that ships them.
struct GeometryBytes {
    vertices: Vec<u8>,
    indices: Vec<u8>,
}

/// One frame's staged work: the two uniform blobs and whichever
/// geometries moved. Overwritten every develop — a frame the render cap
/// could not serve is dropped rather than queued, since the next frame's
/// answer supersedes it anyway.
struct Staged {
    canvas: Canvas,
    bake_uniforms: Vec<u8>,
    wash_uniforms: Vec<u8>,
    /// The drawing, re-solved for this eye.
    ink: GeometryBytes,
    /// The chart's aperture loops, re-projected for this eye.
    aperture: GeometryBytes,
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
    window: Option<(u32, u32)>,
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
    /// The subject's vertex buffer, waiting for the create that ships it.
    /// Packed once per subject: nothing in it turns with the view.
    subject_geometry: Option<GeometryBytes>,
    /// What the subject measures independent of the view, for the
    /// centroids the accidents are placed about.
    survey: Option<Survey>,
    /// The accident stream, rolled for this seed, canvas and visible set.
    seed_slice: Option<SeedUniforms>,
    /// This frame's develop, staged between the CPU half and the mail.
    frame: Option<Staged>,
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
    /// A canvas change orphans every plane pulped for the old one — the
    /// paper's grain is sampled at the canvas' own rate — so it releases
    /// the whole texture set and the seed slice rolled against it. The
    /// programs and the resident geometry survive: neither knows the
    /// canvas size.
    pub fn resized(&mut self, width: u32, height: u32) {
        if self.window == Some((width, height)) {
            return;
        }
        self.window = Some((width, height));
        self.seed_slice = None;
    }

    /// A new subject or field arrived; everything measured off the old one
    /// has to be measured again.
    pub fn subject_changed(&mut self) {
        self.survey = None;
        self.subject_geometry = None;
        self.geometries[SUBJECT_GEOMETRY] = Resident::Absent;
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
    /// order — the subject's, the drawing's, the chart's. `Err` disables
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
        let (width, height) = self.window?;
        let (width, height) = ((width as usize).max(1), (height as usize).max(1));

        let long = width.max(height);
        if long <= CANVAS_LONG_EDGE {
            return Some(Canvas { width, height });
        }
        let scale = CANVAS_LONG_EDGE as f32 / long as f32;
        Some(Canvas {
            width: ((width as f32 * scale) as usize).max(1),
            height: ((height as f32 * scale) as usize).max(1),
        })
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
    /// Every frame, unconditionally: nothing here scales with the canvas
    /// any more, so there is no rasterize to keep off the frame cadence
    /// and no reason for the paint to lag the drawing it sits under. What
    /// this does is measure the subject if it has not been measured, roll
    /// the accident stream if the visible set has changed, and stage the
    /// two uniform blobs and the two geometries that move.
    pub fn develop(&mut self, subject: &Subject<'_>, view: &View) {
        if self.disabled {
            return;
        }
        let Some(canvas) = self.canvas() else {
            return;
        };
        let (body_width, body_height) = canvas.body();
        let Subject { mesh, scores, settings, drawn, chart } = subject;
        // Past the gate, so this is the one place the CPU still splits
        // the drawing into visible runs — once per develop, not once
        // per frame.
        let drawn = &drawn();

        let survey = self.survey.get_or_insert_with(|| Survey::measure(mesh, scores));
        if self.subject_geometry.is_none() && self.geometries[SUBJECT_GEOMETRY] == Resident::Absent {
            self.subject_geometry =
                Some(GeometryBytes { vertices: bake::vertices(mesh, scores, settings), indices: bake::indices(mesh) });
        }

        // Where each material sits, off the surface that projects into it
        // rather than off the pixels it covered — the class plane lives on
        // the GPU and ADR-0170 declines a readback (see `survey`).
        let centroids = survey.centroids(mesh, view.eye, &view.view_proj, body_width, body_height);

        // The chart, asked afresh per frame rather than cached with the
        // anchors: gaze moves the iris inside its own aperture, so a frame
        // solved once at load would leave the paint looking where she used
        // to look.
        let frames = chart
            .as_ref()
            .map(|chart| chart::eye_frames(chart.mesh, chart.anchors, chart.face, settings.eye_style))
            .unwrap_or_default();
        let fine_eyes = accent::project(&frames, &view.view_proj, canvas.width, canvas.height);
        let body_eyes = accent::project(&frames, &view.view_proj, body_width, body_height);
        let presence: Vec<f32> =
            fine_eyes.iter().map(|eye| survey::presence(mesh, view.eye, &frames[eye.frame()].aperture)).collect();

        let stains = stain_centres(&centroids, body_height);
        let placement = Placement { centroids: &centroids, stains: &stains, iris: iris_centre(&fine_eyes) };
        let wanted = Presence::of(&placement);

        // A canvas of a new height wants its own graph: how far the water
        // reaches in pixels is what decides the extent each blur sweeps
        // at, so the structure follows the height and a resize re-lays it.
        // The bake graph knows no canvas and survives the re-lay.
        if self.program.as_ref().is_none_or(|program| program.canvas_height() != canvas.height) {
            self.program = None;
            self.stale_programs.extend(self.programs[WASH].take());
            for sent_for in &mut self.registering {
                if *sent_for == Registering::Wash {
                    *sent_for = Registering::Stale;
                }
            }
        }
        let program = self.program.get_or_insert_with(|| wash::program(canvas.height));
        let slice = self
            .seed_slice
            .take()
            .filter(|slice| slice.serves(SHEET_SEED, canvas, wanted))
            .unwrap_or_else(|| program.seed_uniforms(SHEET_SEED, canvas, wanted));

        let faces = chart.as_ref().map(|_| Faces { fine: &fine_eyes, body: &body_eyes, presence: &presence });
        let frame = Frame { view_proj: view.view_proj, placement, faces };

        self.frame = Some(Staged {
            canvas,
            bake_uniforms: bake::BakeUniforms { view_proj: view.view_proj, eye: view.eye }.encode().to_vec(),
            wash_uniforms: program.frame_uniforms(&slice, &frame),
            ink: GeometryBytes { vertices: ink::vertices(drawn), indices: ink::indices(drawn) },
            aperture: GeometryBytes {
                vertices: face::vertices(&fine_eyes, canvas.width, canvas.height),
                indices: face::indices(&fine_eyes, canvas.width, canvas.height),
            },
        });
        self.seed_slice = Some(slice);
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
        if self.disabled || !self.registering.is_empty() || self.frame.is_none() {
            return Vec::new();
        }
        let Some(program) = self.program.as_ref() else {
            return Vec::new();
        };

        let mut registers = Vec::with_capacity(PROGRAMS);
        if self.programs[BAKE].is_none() {
            self.registering.push_back(Registering::Bake);
            registers.push(bake::program());
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
        let stale = self
            .textures
            .as_ref()
            .zip(self.frame.as_ref())
            .is_some_and(|((_, canvas), staged)| *canvas != staged.canvas);
        if !stale {
            return Vec::new();
        }

        let Some((bindings, _)) = self.textures.take() else {
            return Vec::new();
        };
        bindings.to_vec().into_iter().map(|texture_id| DestroyTexture { texture_id }).collect()
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
            return Vec::new();
        }
        let Some(staged) = self.frame.as_ref() else {
            return Vec::new();
        };
        let canvas = staged.canvas;
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
    /// collects their ids: the subject the bake rasterizes, the drawing
    /// the ink pass does, and the chart's aperture loops the face pass
    /// fills. One create each, then updated in place for the session.
    pub fn take_geometry_creates(&mut self) -> Vec<CreateGeometry> {
        if self.disabled {
            return Vec::new();
        }
        let Some(staged) = self.frame.as_ref() else {
            return Vec::new();
        };

        let mut creates = Vec::new();
        let mut claim = |slot: usize, layout, bytes: &GeometryBytes| {
            creates.push(CreateGeometry { layout, vertices: bytes.vertices.clone(), indices: bytes.indices.clone() });
            slot
        };
        // One create in flight at a time: the reply carries no slot, so
        // the collector matches it against the one slot that is asking.
        if self.geometries.contains(&Resident::Creating) {
            return Vec::new();
        }
        if self.geometries[SUBJECT_GEOMETRY] == Resident::Absent
            && let Some(subject) = self.subject_geometry.as_ref()
        {
            self.geometries[claim(SUBJECT_GEOMETRY, bake::geometry_slot().layout, subject)] = Resident::Creating;
            return creates;
        }
        if self.geometries[INK_GEOMETRY] == Resident::Absent {
            self.geometries[claim(INK_GEOMETRY, ink::geometry_slot().layout, &staged.ink)] = Resident::Creating;
            return creates;
        }
        if self.geometries[APERTURE_GEOMETRY] == Resident::Absent {
            self.geometries[claim(APERTURE_GEOMETRY, face::geometry_slot().layout, &staged.aperture)] =
                Resident::Creating;
            return creates;
        }

        Vec::new()
    }

    /// The two geometries that move with the eye, replacing the resident
    /// bytes in place. The subject's does not: nothing in its buffer turns
    /// with the view, so it is uploaded once and left alone.
    pub fn take_geometry_updates(&mut self) -> Vec<UpdateGeometry> {
        let Some(staged) = self.frame.as_ref() else {
            return Vec::new();
        };

        [(INK_GEOMETRY, &staged.ink), (APERTURE_GEOMETRY, &staged.aperture)]
            .into_iter()
            .filter_map(|(slot, bytes)| {
                self.geometries[slot].id().map(|geometry_id| UpdateGeometry {
                    geometry_id,
                    vertices: bytes.vertices.clone(),
                    indices: bytes.indices.clone(),
                })
            })
            .collect()
    }

    /// The two dispatches developing this frame: the bake filling the
    /// packed plane off the subject's own geometry, then the wash reading
    /// it. Sent after the geometry updates they read, in the same frame,
    /// and in this order — the wash samples what the bake wrote.
    pub fn take_dispatch(&mut self) -> Vec<ProgramDispatch> {
        let (Some(bake_id), Some(wash_id)) = (self.programs[BAKE], self.programs[WASH]) else {
            return Vec::new();
        };
        let Some((bindings, canvas)) = self.textures.as_ref() else {
            return Vec::new();
        };
        let live: Option<Vec<u32>> = self.geometries.iter().map(|at| at.id()).collect();
        let (Some(ids), Some(staged)) = (live, self.frame.take()) else {
            return Vec::new();
        };
        if staged.canvas != *canvas {
            return Vec::new();
        }

        self.developed = true;
        vec![
            ProgramDispatch {
                program_id: bake_id,
                bindings: vec![bindings.packed],
                geometries: vec![ids[SUBJECT_GEOMETRY]],
                uniforms: staged.bake_uniforms,
            },
            ProgramDispatch {
                program_id: wash_id,
                bindings: bindings.to_vec(),
                geometries: vec![ids[INK_GEOMETRY], ids[APERTURE_GEOMETRY]],
                uniforms: staged.wash_uniforms,
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
fn stain_centres(centroids: &[Option<Vec2>; survey::SLOTS], height: usize) -> [Option<Vec2>; survey::SLOTS] {
    let mut stains = [None; survey::SLOTS];
    for material in palette::MATERIALS {
        let Some(policy) = material.atmosphere.as_ref() else {
            continue;
        };
        let drift = Vec2::new(image::tuned(policy.drift.0, height), image::tuned(policy.drift.1, height));
        stains[usize::from(material.class)] =
            centroids.get(usize::from(material.class)).copied().flatten().map(|centre| centre + drift);
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

    use crate::labels::CLASSES;
    use crate::mesh::Mesh;

    /// A quad standing well inside the frame, every vertex skin.
    fn quad() -> Mesh {
        Mesh::from_obj_bytes(b"v -0.5 -0.5 0\nv 0.5 -0.5 0\nv 0.5 0.5 0\nv -0.5 0.5 0\nf 1 2 3\nf 1 3 4\n", 0)
            .expect("fixture mesh")
    }

    fn view() -> View {
        let eye = Vec3::new(0.0, 0.0, 5.0);
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
        let drawn = Vec::new;

        let mut easel = Easel::default();
        easel.resized(160, 200);

        let mut next_id = 0u32;
        let mut ids = || {
            next_id += 1;
            next_id
        };
        // Ten render stages is far more than the protocol needs — the
        // point is that it converges, not how fast.
        for _ in 0..10 {
            let subject = Subject { mesh: &mesh, scores: &scores, settings: &settings, drawn: &drawn, chart: None };
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

    // Tripwire: a canvas change must release the paper pulped for the old
    // one. The paper's grain is sampled at the canvas' own rate, so a
    // sheet kept across a resize granulates against a texture of the wrong
    // size — and the shipped bug this pins is its ancestor (issue 4391 —
    // the displaced ear flush after fullscreening).
    #[test]
    fn a_resize_orphans_the_seed_slice_and_a_same_size_announcement_keeps_it() {
        let mut easel = Easel::default();
        easel.resized(800, 1000);
        let canvas = easel.canvas().expect("a resized easel has a canvas");
        easel.seed_slice = Some(wash::program(canvas.height).seed_uniforms(SHEET_SEED, canvas, Presence::default()));

        easel.resized(800, 1000);
        assert!(easel.seed_slice.is_some(), "a same-size announcement must not orphan the accident stream");

        easel.resized(3024, 1670);
        assert!(easel.seed_slice.is_none(), "a canvas change must, so the next develop re-rolls at the new size");
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
