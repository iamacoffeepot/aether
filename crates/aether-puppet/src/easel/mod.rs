//! The easel: watercolour laid under the drawing
//! (iamacoffeepot/aether#4349).
//!
//! The wash never runs at frame rate. Each render stage costs one textured
//! material rect — a sheet of painted paper standing behind the subject,
//! re-oriented to face the eye every frame — and the painting on it is
//! re-developed only when the view has settled somewhere new. The ink is
//! solved live above it and wins the depth test, so during an orbit the
//! drawing moves over a held painting, and a couple of seconds after the
//! camera rests the sheet catches up. Repaint on the twos: the paint
//! re-develops rather than smears.
//!
//! [`regions`] bakes the painter's input maps through the drawing's own
//! camera; [`field`] turns a region into wet paint on the CPU — the
//! parity oracle — while [`program`] speaks the same develop as one
//! registered ADR-0170 render program; [`palette`] owns the pigments.
//! This module is the orchestrator: when to develop, what to upload, and
//! where the sheet stands. A due repaint rasterizes the planes, flow and
//! accents on the CPU exactly as before, but the coats and the composite
//! run on the GPU: the planes upload as `R32Float` registry textures,
//! the accidents ride the uniform blob, and one dispatch develops the
//! writable `Rgba8` sheet the billboard samples.

pub mod accent;
#[cfg(test)]
mod crossfeed;
pub mod field;
pub mod image;
pub mod palette;
pub mod program;
pub mod regions;

use aether_math::{Mat4, Vec3};
use aether_render::{
    CreateGeometry, CreateTexture, DestroyTexture, DrawMaterialTextured, DrawTriangle, MaterialRect,
    MaterialTexturedRect, ProgramDispatch, ProgramRegister, QuadBlend, TextureFormat, TextureSampling, TextureUsage,
    UpdateGeometry, UpdateTexture,
};

use program::wash::{self, WashBindings, WashProgram};

use crate::anchor::Anchors;
use crate::chart::{self, Face};
use crate::extract::Settings;
use crate::labels;
use crate::mesh::Mesh;

/// Frames the view must hold still before the sheet re-develops — about
/// two seconds at the chassis' 60 Hz cadence. The first painting of a
/// subject skips the wait; there is nothing on the sheet to hold.
const SETTLE_FRAMES: u32 = 120;

/// Long-edge ceiling on the wash canvas.
///
/// The wash carries only the low frequencies, so it used to develop at
/// half the window's pixels and lose nothing the eye could find. The
/// accents ended that: an iris is a couple of dozen pixels across at this
/// framing and its slit a fraction of one, so at half resolution the eye
/// shape goes and what is left is a blue smudge behind the ink. The sheet
/// now develops at the window's own pixels up to this ceiling — which is
/// the resolution every distance in the engine was tuned at — and the
/// settle gate keeps the extra cost off the frame path.
const CANVAS_LONG_EDGE: usize = 1280;

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
/// solved for this eye (the flow's source), and the chart when the
/// subject has a face. Borrowed for the call — the easel keeps none of
/// it.
pub struct Subject<'a> {
    pub mesh: &'a Mesh,
    pub scores: &'a [[f32; labels::CLASSES]],
    pub settings: &'a Settings,
    pub drawn: &'a [DrawTriangle],
    pub chart: Option<Chart<'a>>,
}

/// The plane textures one develop uploads, in [`WashBindings`]
/// declaration order; the writable sheet makes the binding count.
const PLANE_COUNT: usize = 12;

/// One develop's data, staged between the CPU rasterize and the mail
/// that carries it: the plane pixel buffers in binding order and the
/// uniform blob the dispatch windows into.
struct Develop {
    width: usize,
    height: usize,
    planes: Vec<Vec<u8>>,
    uniforms: Vec<u8>,
}

/// The ribbon geometry for one develop, packed for the ink pass' vertex
/// layout and waiting for the mail that carries it.
struct InkGeometry {
    vertices: Vec<u8>,
    indices: Vec<u8>,
}

/// Where the resident ribbon geometry stands: nothing sent yet, a create
/// in flight whose id has not come back, or the id the render cap
/// assigned. Only the last can be dispatched against, and only the first
/// may send a create — the same hold the plane creates keep.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Resident {
    #[default]
    Absent,
    Creating,
    Live(u32),
}

/// The creates for one canvas size are in flight; the render cap
/// answers them in send order, so the collected ids land in binding
/// order. The uniforms wait here for the dispatch that follows.
struct Creating {
    ids: Vec<u32>,
    size: (usize, usize),
    uniforms: Vec<u8>,
}

/// The wash layer's state machine: a developed repaint waiting to
/// upload, the registered program and plane textures the render cap
/// assigned, and the settle gate between paintings.
#[derive(Default)]
pub struct Easel {
    window: Option<(u32, u32)>,
    /// The eye on the previous render stage, for the settle gate: motion
    /// resets the count, stillness accumulates it.
    last_seen: Option<Vec3>,
    frames_still: u32,
    painted_from: Option<Vec3>,
    /// The wash program's static graph and window layout, laid once at
    /// the first develop and kept for the session.
    program: Option<WashProgram>,
    /// A register is in flight; hold further registers until it answers.
    registering: bool,
    /// The program id the render cap assigned.
    program_id: Option<u32>,
    /// A develop not yet on the GPU.
    pending: Option<Develop>,
    /// The bound registry textures and the size they were created at.
    textures: Option<(WashBindings, (usize, usize))>,
    /// Creates are in flight; hold further creates until they answer.
    creating: Option<Creating>,
    /// A develop's uniforms whose planes are already staged, waiting for
    /// the dispatch mail.
    dispatch_uniforms: Option<Vec<u8>>,
    /// This develop's ribbon geometry, waiting for the create or update
    /// that ships it.
    ink: Option<InkGeometry>,
    /// Where that geometry stands with the render cap.
    resident_ink: Resident,
    /// At least one dispatch has been sent, so the sheet holds paint
    /// rather than the writable texture's transparent clear.
    developed: bool,
    /// The render cap refused the register or a create — the headless
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
    /// A canvas change orphans the painted sheet — the old pixels would
    /// stretch over the new frustum — so it invalidates exactly as a
    /// subject change does, with first-paint-immediate semantics on the
    /// next render. A same-size announcement keeps the sheet.
    pub fn resized(&mut self, width: u32, height: u32) {
        if self.window == Some((width, height)) {
            return;
        }
        self.window = Some((width, height));
        self.painted_from = None;
    }

    /// A new subject or field arrived; the sheet no longer describes it.
    pub fn subject_changed(&mut self) {
        self.painted_from = None;
    }

    /// The reply to a requested register. `Err` disables the easel for
    /// the session, exactly as a refused create does.
    pub fn registered(&mut self, result: Result<u32, ()>) {
        self.registering = false;
        match result {
            Ok(program_id) => self.program_id = Some(program_id),
            Err(()) => self.disabled = true,
        }
    }

    /// The reply to one requested create. Ids arrive in send order —
    /// binding order — and the thirteenth completes the set.
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
        let Creating { ids, size, uniforms } = creating;
        let bindings = WashBindings {
            classes: ids[0],
            tone: ids[1],
            care: ids[2],
            tooth: ids[3],
            edge: ids[4],
            paper_shade: ids[5],
            flow_x: ids[6],
            flow_y: ids[7],
            coherence: ids[8],
            lift: ids[9],
            iris: ids[10],
            blush: ids[11],
            sheet: ids[12],
        };
        self.textures = Some((bindings, size));
        self.dispatch_uniforms = Some(uniforms);
    }

    /// The reply to the one requested geometry create. `Err` disables the
    /// easel for the session, as a refused plane create does — the ink
    /// pass is part of the graph, so a program without it develops
    /// nothing worth showing.
    pub fn ink_created(&mut self, result: Result<u32, ()>) {
        let Ok(geometry_id) = result else {
            self.resident_ink = Resident::Absent;
            self.disabled = true;
            return;
        };
        self.resident_ink = Resident::Live(geometry_id);
    }

    /// The first develop's ribbon geometry: created once to earn an id,
    /// then re-shipped by [`Easel::take_ink_update`] for the life of the
    /// session.
    pub fn take_ink_create(&mut self) -> Option<CreateGeometry> {
        if self.disabled || self.resident_ink != Resident::Absent {
            return None;
        }
        let InkGeometry { vertices, indices } = self.ink.take()?;

        self.resident_ink = Resident::Creating;
        Some(CreateGeometry { layout: program::ink::geometry_slot().layout, vertices, indices })
    }

    /// Every later develop's ribbon geometry, replacing the resident
    /// bytes in place. The layout is fixed at create, so only the
    /// contents travel.
    pub fn take_ink_update(&mut self) -> Option<UpdateGeometry> {
        let Resident::Live(geometry_id) = self.resident_ink else {
            return None;
        };
        let InkGeometry { vertices, indices } = self.ink.take()?;

        Some(UpdateGeometry { geometry_id, vertices, indices })
    }

    /// The window's own pixels, clamped to the long-edge ceiling.
    fn canvas(&self) -> Option<(usize, usize)> {
        let (width, height) = self.window?;
        let (width, height) = ((width as usize).max(1), (height as usize).max(1));

        let long = width.max(height);
        if long <= CANVAS_LONG_EDGE {
            return Some((width, height));
        }
        let scale = CANVAS_LONG_EDGE as f32 / long as f32;
        Some((((width as f32 * scale) as usize).max(1), ((height as f32 * scale) as usize).max(1)))
    }

    /// The exact planes a develop at the current view would paint from,
    /// for the dump diagnostic — same canvas clamp, same rasterize, no
    /// settle gate and no paint. `None` before the first resize.
    pub fn bake_planes(&self, subject: &Subject<'_>, view: &View) -> Option<regions::RegionPlanes> {
        let (width, height) = self.canvas()?;

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

    /// Re-develop the sheet if the view has settled somewhere unpainted.
    /// The CPU keeps only the rasterize — planes, flow, accents, the
    /// paper's noise — and the accident stream the uniform blob replays;
    /// the coats and the composite run as the registered wash program,
    /// so the render handler no longer blocks on the blurs. The gate is
    /// what keeps even the rasterize off every frame: never mid-drag,
    /// never twice for one view, never before the eye has rested.
    pub fn develop(&mut self, subject: &Subject<'_>, view: &View, dragging: bool) {
        if self.disabled {
            return;
        }

        if dragging || self.last_seen != Some(view.eye) {
            self.frames_still = 0;
        } else {
            self.frames_still += 1;
        }
        self.last_seen = Some(view.eye);
        if self.painted_from == Some(view.eye) || (self.painted_from.is_some() && self.frames_still < SETTLE_FRAMES) {
            return;
        }
        let Some((width, height)) = self.canvas() else {
            return;
        };
        let Subject { mesh, scores, settings, drawn, chart } = subject;

        let regions = regions::rasterize(mesh, scores, settings, view.eye, &view.view_proj, width, height);
        let planes =
            field::Planes { classes: &regions.class, tone: &regions.tone, facing: &regions.facing, width, height };

        // Asked of the chart per repaint rather than cached with the
        // anchors: gaze moves the iris inside its own aperture, so a frame
        // solved once at load would leave the paint looking where she used
        // to look.
        let accents = chart.as_ref().map(|chart| {
            let frames = chart::eye_frames(chart.mesh, chart.anchors, chart.face, settings.eye_style);

            accent::paint(&frames, &view.view_proj, &planes)
        });

        // Gesture follows form: the drawing already knows which way the
        // hair runs, so the wash asks it rather than guessing. Baked here
        // and dropped here — it describes this view of this drawing, and a
        // view that moves invalidates it along with the sheet.
        let flow = image::structure_tensor_flow(&regions::ink(drawn, &view.view_proj, width, height), width, height);

        let sheet = field::Sheet::new(planes, SHEET_SEED);
        let program = self.program.get_or_insert_with(wash::program);
        let uniforms = program.uniforms(&sheet, Some(&flow), accents.as_ref(), view.view_proj);

        // The same triangles the CPU rasterize above just walked, staged
        // for the ink pass to rasterize on the GPU. Re-shipped every
        // develop rather than registered once: the drawing is solved
        // fresh for each eye, and a posed mesh will solve it per frame.
        self.ink = Some(InkGeometry { vertices: program::ink::vertices(drawn), indices: program::ink::indices(drawn) });

        // The plane pixel buffers, in binding order. An absent chart
        // uploads zero planes for the accent inputs — the uniforms
        // already neutralize the passes that would read them.
        let zero = || vec![0u8; width * height * 4];
        let accent_planes = accents.as_ref().and_then(|accents| {
            accents
                .mask(palette::IRIS)
                .map(|iris| [plane_bytes(&accents.lift), plane_bytes(iris), plane_bytes(&accents.blush)])
        });
        let [lift, iris, blush] = accent_planes.unwrap_or_else(|| [zero(), zero(), zero()]);
        let plane_buffers = vec![
            regions.class.iter().flat_map(|&class| f32::from(class).to_le_bytes()).collect(),
            plane_bytes(&regions.tone),
            plane_bytes(sheet.care()),
            plane_bytes(&sheet.noise().tooth),
            plane_bytes(&sheet.noise().edge),
            plane_bytes(sheet.paper_shade()),
            plane_bytes(&flow.x),
            plane_bytes(&flow.y),
            plane_bytes(&flow.coherence),
            lift,
            iris,
            blush,
        ];

        self.pending = Some(Develop { width, height, planes: plane_buffers, uniforms });
        self.painted_from = Some(view.eye);
        self.frames_still = 0;
    }

    /// The register mail carrying the wash program, once per session at
    /// the first develop — by then the GPU device exists, since a develop
    /// only ever follows a render stage. The reply lands in
    /// [`Easel::registered`].
    pub fn take_register(&mut self) -> Option<&ProgramRegister> {
        if self.disabled || self.registering || self.program_id.is_some() || self.pending.is_none() {
            return None;
        }

        let register = self.program.as_ref()?.register();
        self.registering = true;
        Some(register)
    }

    /// The textures whose size no longer matches the pending develop, to
    /// release before the next creates. Resize is the only path here, and
    /// it releases the whole set — the plane textures and the sheet.
    pub fn take_destroys(&mut self) -> Vec<DestroyTexture> {
        if self.creating.is_some() {
            return Vec::new();
        }
        let stale = self
            .textures
            .as_ref()
            .zip(self.pending.as_ref())
            .is_some_and(|((_, size), pending)| *size != (pending.width, pending.height));
        if !stale {
            return Vec::new();
        }

        // Uniforms staged for the released textures window a develop at
        // the old size; the pending develop re-stages its own.
        self.dispatch_uniforms = None;
        let Some((bindings, _)) = self.textures.take() else {
            return Vec::new();
        };
        bindings.to_vec().into_iter().map(|texture_id| DestroyTexture { texture_id }).collect()
    }

    /// The creates carrying the first develop at this size: the plane
    /// pixels ride the creates themselves, the writable sheet is created
    /// empty, and the uniforms wait for the dispatch that follows once
    /// every id has answered.
    pub fn take_creates(&mut self) -> Vec<CreateTexture> {
        if self.disabled || self.program_id.is_none() || self.creating.is_some() || self.textures.is_some() {
            return Vec::new();
        }
        let Some(develop) = self.pending.take() else {
            return Vec::new();
        };

        let (width, height) = (develop.width as u32, develop.height as u32);
        let mut creates: Vec<CreateTexture> = develop
            .planes
            .into_iter()
            .map(|pixels| CreateTexture {
                width,
                height,
                format: TextureFormat::R32Float,
                sampling: TextureSampling::Nearest,
                usage: TextureUsage::Sampled,
                pixels,
            })
            .collect();
        creates.push(CreateTexture {
            width,
            height,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        });

        self.creating =
            Some(Creating { ids: Vec::new(), size: (develop.width, develop.height), uniforms: develop.uniforms });
        creates
    }

    /// A freshly developed repaint over existing same-size textures: one
    /// update per plane, with the uniforms staged for the dispatch that
    /// follows in the same frame.
    pub fn take_updates(&mut self) -> Vec<UpdateTexture> {
        let matches = self
            .textures
            .as_ref()
            .zip(self.pending.as_ref())
            .is_some_and(|((_, size), pending)| *size == (pending.width, pending.height));
        if !matches {
            return Vec::new();
        }

        let (Some(develop), Some((bindings, _))) = (self.pending.take(), self.textures.as_ref()) else {
            return Vec::new();
        };
        let (width, height) = (develop.width as u32, develop.height as u32);
        let updates = bindings
            .to_vec()
            .into_iter()
            .zip(develop.planes)
            .map(|(texture_id, pixels)| UpdateTexture { texture_id, x: 0, y: 0, width, height, pixels })
            .collect();

        self.dispatch_uniforms = Some(develop.uniforms);
        updates
    }

    /// The dispatch developing the staged planes into the sheet. Sent
    /// after the updates it reads, in the same frame — the program's
    /// passes record before the material pass that samples the sheet, so
    /// the billboard shows this develop, not the last one.
    pub fn take_dispatch(&mut self) -> Option<ProgramDispatch> {
        let program_id = self.program_id?;
        let (bindings, _) = self.textures.as_ref()?;
        // Checked before the uniforms are taken, so a develop whose
        // geometry create is still in flight keeps them for the frame
        // that can use them rather than dispatching without its ink.
        let Resident::Live(geometry_id) = self.resident_ink else {
            return None;
        };
        let uniforms = self.dispatch_uniforms.take()?;

        self.developed = true;
        Some(ProgramDispatch { program_id, bindings: bindings.to_vec(), geometries: vec![geometry_id], uniforms })
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

#[cfg(test)]
mod tests {
    use super::*;

    // Tripwire: a canvas change must invalidate the painted sheet. The
    // shipped bug this pins: `resized` recorded the new window but kept
    // `painted_from`, so with an unmoved eye the develop gate skipped
    // forever and the old sheet stretched over the new frustum (issue
    // 4391 — the displaced ear flush after fullscreening).
    #[test]
    fn a_resize_invalidates_the_sheet_and_a_same_size_announcement_keeps_it() {
        let mut easel = Easel::default();
        easel.resized(800, 1000);
        easel.painted_from = Some(Vec3::new(0.0, 0.0, 5.4));

        easel.resized(800, 1000);
        assert!(easel.painted_from.is_some(), "a same-size announcement must not force a repaint");

        easel.resized(3024, 1670);
        assert!(easel.painted_from.is_none(), "a canvas change must orphan the sheet so the next render repaints");
    }
}
