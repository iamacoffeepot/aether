//! Getting the drawing onto the GPU and back into the frame.
//!
//! The wash has the [`easel`](crate::easel) for this; the ink has this.
//! The two are shaped alike — register once, create per canvas size,
//! dispatch per repaint, present as a rect — and differ in cadence,
//! which is the whole reason they are separate state machines.
//!
//! The easel develops every frame. The ink does not, and does not need
//! to: its planes and its sheet are a function of the drawing, the view
//! and the canvas, so a frame that moved none of the three would spend
//! forty-eight passes deriving what is already standing in its own
//! textures. What that costs the layer is the bookkeeping to say when
//! one of them moved — a revision, and the revision the planes standing
//! there were produced from — and everything else here is sized for the
//! frames that did move one: geometry replaced in place
//! rather than recreated, only the volatile half of it travelling, and
//! the uniform blobs written at dispatch time, which is where the
//! canvas is known.
//!
//! Two programs run, in order, against one set of textures:
//!
//! 1. [`sight`] writes the four field planes at canvas resolution —
//!    the point's verdict, its reach, its curve's coverage, its run's
//!    arc.
//! 2. [`stroke`] reads those planes in its vertex stage, rasterizes
//!    the ribbons into a supersampled ink sheet, and reduces that same
//!    raster into the wash's ink coverage plane on the way past.
//!
//! The sheet then composites as a screen-space quad, which puts it in
//! the overlay pass — after the material pass the wash sheet draws in,
//! and with no depth of its own. That ordering is what places the ink
//! in front of the wash, and it is why the ink's own depth test lives
//! inside its program rather than in the frame.

use std::mem;

use aether_math::{Mat4, Rgba, Vec3};
use aether_render::{
    CreateGeometry, CreateTexture, DestroyTexture, DrawMaterialTextured, MaterialRect, MaterialTexturedRect,
    ProgramDispatch, ProgramRegister, QuadBlend, TextureFormat, TextureSampling, TextureUsage, UpdateGeometry,
    VertexAttribute,
};

use crate::deform::{BONE_LIMIT, Bound, Skin};
use crate::easel::program::sight::ToneUniforms;
use crate::easel::program::wash::BODY_DIVISOR;
use crate::easel::program::{sight, stroke};
use crate::easel::{View, wash_canvas};
use crate::feature::{Drawing, Half};
use crate::mesh::Mesh;

/// The pose a frame is drawn at, as the two ink programs take it.
///
/// One value rather than three arguments because the three have to agree
/// or the drawing comes apart: the bone table both dispatches skin from,
/// the sculpt-and-rig the resident curves are packed against, and the
/// tone gate that reads the normals the same table turned. `bound` is
/// `None` for a subject with no rig, which is also the only case where
/// the gate is already settled on the CPU.
#[derive(Clone, Copy)]
pub struct Posing<'a> {
    pub bound: Option<Bound<'a>>,
    pub bones: [f32; BONE_LIMIT * 12],
    pub tone: ToneUniforms,
}

/// A resource the render cap assigns an id to: nothing asked yet, an
/// ask in flight, or the id it answered with.
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
            Self::Live(id) => Some(id),
            _ => None,
        }
    }
}

/// Ids collected in send order. The render cap answers a batch in the
/// order it was asked, so position in this vector is identity — the
/// same convention the easel's plane creates use.
#[derive(Default)]
struct Ordered {
    ids: Vec<u32>,
    asking: bool,
}

impl Ordered {
    fn ready(&self, wanted: usize) -> bool {
        self.ids.len() == wanted
    }

    fn answered(&mut self, id: u32, wanted: usize) {
        self.ids.push(id);
        if self.ids.len() == wanted {
            self.asking = false;
        }
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.asking = false;
    }
}

/// One geometry's packed vertices and indices, staged together because
/// nothing ever ships one without the other.
type Packed = (Vec<u8>, Vec<u8>);

/// One frame's solved drawing, staged between the CPU solve and the
/// mail that carries it.
///
/// The resident half is not here. It belongs to the subject rather than
/// to the frame, and it is staged in [`Strokes::resident`] beside the
/// subject's own mesh.
///
/// Taken by whichever of the creates and the updates ships it, so a
/// staging can travel once and only once — the creates move the same
/// buffers the updates would, and shipping the emptied vectors
/// afterwards would replace the resident drawing with nothing.
struct Solved {
    points: Packed,
    ribbons: Packed,
    /// One texel per curve, over the whole drawing rather than one
    /// half: [`ribbon::reference_depth`] is per eye for a resident
    /// curve exactly as for a volatile one.
    ///
    /// [`ribbon::reference_depth`]: crate::ribbon::reference_depth
    curves: Packed,
}

/// The view the drawing on the GPU was solved through, and how much of
/// the field it laid out into.
///
/// Held past the dispatch that carried it, which is what lets the layer
/// re-fill its planes without a fresh solve. Nothing a point or a ribbon
/// vertex carries is a function of the canvas — a packed slot is the
/// flat texel index and the shader divides it by the field's own width
/// from the uniform — so a resize needs new textures, a re-encoded blob,
/// and a dispatch, and no re-pack at all.
#[derive(Clone, Copy)]
struct Standing {
    view_proj: Mat4,
    eye: Vec3,
    bias: f32,
    /// The pose the standing drawing is posed to, and the gate its
    /// hatching is read through. Both ride the dispatch's uniform blob,
    /// which is the whole of what a pose ships
    /// (iamacoffeepot/aether#4462).
    bones: [f32; BONE_LIMIT * 12],
    tone: ToneUniforms,
    /// Texels the layout occupied, gaps included. A canvas the drawing
    /// outgrew cannot be dispatched into: the texels past its end are
    /// not addressable, so the strokes that landed there would go
    /// silently missing rather than arrive smaller.
    occupied: usize,
}

/// The subject's own packed drawing, waiting for the create or update.
///
/// Both halves of it are functions of the surface rather than of the
/// eye — the field's points were already (#4435), and the ribbons are
/// now that the rail solve reads the camera in the vertex stage
/// (#4440) — so this is packed once per subject and then left on the
/// GPU for as long as the subject stands.
struct Staged {
    points: Packed,
    ribbons: Packed,
}

/// The ink layer's state machine.
#[derive(Default)]
pub struct Strokes {
    window: Option<(u32, u32)>,
    /// The canvas the field and the sheet are sized to — the window
    /// when one has been announced, and otherwise a frustum-shaped
    /// stand-in. Resolved at solve time and held for the creates.
    canvas: Option<(u32, u32)>,
    programs: Ordered,
    textures: Ordered,
    /// The canvas the current textures were created at.
    sized: Option<(u32, u32)>,
    geometries: Ordered,
    subject: Resident,
    /// The subject's packed bytes, waiting for the create.
    ///
    /// Once. The prepass' vertex stage poses the subject from the bone
    /// table, so the buffer is the rest sculpt's and there is nothing a
    /// pose can invalidate about it.
    subject_bytes: Option<Packed>,
    /// The resident half's packed bytes, waiting for the same create or
    /// update the subject's do. Staged by the first solve after the
    /// subject changed rather than by `subject_changed` itself, because
    /// the pack needs the layout and the layout needs the curves the new
    /// subject was extracted into.
    resident: Option<Staged>,
    /// Whether the resident half is still owed a re-pack. Set with the
    /// subject, cleared by the solve that packs it.
    resident_stale: bool,
    solved: Option<Solved>,
    /// The view the drawing standing on the GPU was solved through.
    standing: Option<Standing>,
    /// What the field's planes and the ink sheet would be produced from
    /// now. Bumped by every input either of them is a function of — a
    /// solve (a new drawing at a new view) and a canvas change.
    revision: u64,
    /// The revision the standing planes and sheet *were* produced from,
    /// and `None` when they hold nothing — before the first dispatch,
    /// and again after a resize releases the textures.
    ///
    /// This is the whole of the field's cadence
    /// (iamacoffeepot/aether#4448). The chain is 45 passes over the
    /// canvas and its output is a function of the drawing, the view and
    /// the field's extent alone, so a frame that changed none of them
    /// would re-derive the planes it already has. Keyed on the inputs
    /// rather than on elapsed frames, because the skip is only sound
    /// while the planes standing there came from the inputs standing
    /// here — the first frame, a freshly loaded drawing and a resize's
    /// blank textures all have to re-fill, and none of them is a
    /// question about how long it has been.
    dispatched: Option<u64>,
    /// At least one dispatch has landed, so the sheet holds ink rather
    /// than a writable texture's transparent clear.
    drawn: bool,
    /// The render cap refused a register or a create — the headless
    /// chassis' fail-fast reply — so the layer stops asking.
    disabled: bool,
}

/// Long edge of the stand-in canvas used before a window is announced.
const CANVAS_EDGE: u32 = 1200;

/// How far in front of the subject's near side the ink stands.
const INK_STANDOFF: f32 = 0.05;

/// Nearest the ink is ever placed, comfortably clear of the 0.05 near
/// plane.
const INK_DEPTH_FLOOR: f32 = 0.25;

/// Programs registered, in send order.
const PROGRAM_COUNT: usize = 2;
/// The five field planes, the ink sheet, and the wash's ink coverage
/// plane — the stroke program's binding order.
const TEXTURE_COUNT: usize = sight::PLANE_COUNT + 2;
/// Where the sheet and the coverage plane sit among them.
const SHEET: usize = sight::PLANE_COUNT;
const INK_PLANE: usize = sight::PLANE_COUNT + 1;
/// The field's four — subject, resident points, volatile points,
/// curves — then the ink's two ribbon halves.
const GEOMETRY_COUNT: usize = sight::GEOMETRY_COUNT + stroke::GEOMETRY_COUNT;
/// Where the ribbon halves sit among them, in the ink program's own
/// slot order.
const RIBBONS: [usize; stroke::GEOMETRY_COUNT] =
    [sight::GEOMETRY_COUNT + stroke::RESIDENT as usize, sight::GEOMETRY_COUNT + stroke::VOLATILE as usize];

/// The canvas a window of these pixels develops at.
///
/// [`wash_canvas`] and nowhere else. The wash resolves its own canvas the
/// same way, and the two have to land on one because the ink coverage
/// plane this layer creates is a binding of both their programs
/// (iamacoffeepot/aether#4451) — and a program binding's size is checked
/// against the extent the graph was registered at. So two window-to-canvas
/// maps that agreed at one framing would not be an agreement: at a window
/// past the wash's long-edge clamp they part, the wash dispatch that binds
/// the plane is dropped whole in the cap, and the sheet stays the
/// transparent clear it was created as — no error anywhere, just paint
/// that never arrives (iamacoffeepot/aether#4465).
fn windowed_canvas(width: u32, height: u32) -> (u32, u32) {
    let canvas = wash_canvas(width, height);

    (canvas.width as u32, canvas.height as u32)
}

impl Strokes {
    /// A canvas change orphans every texture in the set — the field's
    /// capacity and the sheet's size both follow it.
    ///
    /// The canvas is resolved here rather than waiting for the next
    /// solve. A solve happens when the eye moves; a window is resized
    /// with the camera held all the time, and a canvas that only caught
    /// up at the next solve left the field and the sheet standing at the
    /// old size until something else moved.
    ///
    /// Resolved through `windowed_canvas` and not from the window's own
    /// pixels, because a desktop window announces its size every frame
    /// and this is therefore the *last* writer of the canvas on every
    /// frame that did not solve — so a second way of answering "what
    /// canvas is this window" here is not a near-miss, it is the answer
    /// (iamacoffeepot/aether#4465).
    pub fn resized(&mut self, width: u32, height: u32) {
        self.window = Some((width, height));
        self.recanvas(windowed_canvas(width, height));
    }

    /// Point the layer at a canvas, and note the change if it is one.
    fn recanvas(&mut self, canvas: (u32, u32)) {
        if self.canvas != Some(canvas) {
            self.canvas = Some(canvas);
            self.revision += 1;
        }
    }

    /// The canvas to solve at.
    ///
    /// The window when one has been announced, resolved through
    /// [`wash_canvas`] — the wash resolves its own the same way, and the
    /// two have to land on one canvas because the coverage plane this
    /// layer writes is a binding of both their programs
    /// (iamacoffeepot/aether#4451). Otherwise a stand-in of
    /// the same shape as the frustum, which is all the ink needs: it
    /// composites as a world-space billboard spanning the frustum's own
    /// cross-section, so the sheet's *resolution* is a quality choice
    /// and only its *aspect* has to agree with the view. That is the
    /// difference from a screen-space quad, which cannot be placed
    /// without knowing the window in pixels.
    fn resolve_canvas(&self, aspect: f32) -> (u32, u32) {
        if let Some((width, height)) = self.window {
            return windowed_canvas(width, height);
        }
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        if aspect >= 1.0 {
            (CANVAS_EDGE, ((CANVAS_EDGE as f32 / aspect).round() as u32).max(1))
        } else {
            (((CANVAS_EDGE as f32 * aspect).round() as u32).max(1), CANVAS_EDGE)
        }
    }

    /// A new subject: its geometry has to travel before the next prepass
    /// means anything, and so do the resident points, which are the new
    /// surface's own curves.
    ///
    /// Both travel exactly once now. The subject arrives carrying its
    /// bone binding and the resident curves arrive carrying theirs, so
    /// what a pose changes about either is the uniform blob — which is
    /// what made the pose sweep's whole-drawing re-upload disappear
    /// rather than get faster (iamacoffeepot/aether#4462).
    pub fn subject_changed(&mut self, mesh: &Mesh, skin: Option<&Skin>) {
        self.subject_bytes = Some((sight::subject_vertices(mesh, skin), sight::subject_indices(mesh)));
        self.resident_stale = true;
        self.revision += 1;
    }

    /// Whether the layer can be asked to draw at all. A refused
    /// register (the headless chassis) leaves this false for the
    /// session, and the caller keeps its CPU path.
    #[must_use]
    pub fn live(&self) -> bool {
        !self.disabled
    }

    /// The wash's ink coverage plane, once the set it belongs to stands.
    ///
    /// The wash binds this and reads where the drawing landed out of it
    /// (iamacoffeepot/aether#4451). `None` until every texture in the set
    /// has answered — a partial set has no id at this slot — and again
    /// after a resize releases them, which is what makes the caller
    /// re-read it per frame rather than keep it.
    ///
    /// A plane that stands but has never been dispatched into is
    /// transparent, so a wash developed against it paints without a
    /// drawing for the frame or two before the first dispatch lands,
    /// exactly as the sheet shows no ink over the same frames.
    #[must_use]
    pub fn ink_plane(&self) -> Option<u32> {
        self.textures.ids.get(INK_PLANE).copied()
    }

    /// Solve one frame's drawing for the GPU: lay the field out over
    /// the drawing, pack the points the field is rasterized from and the
    /// ribbons the ink is rasterized from, and stage both uniform
    /// blobs.
    ///
    /// Only the volatile half of either travels. The resident half of
    /// each is packed once per subject and left on the GPU — its curves
    /// do not move, [`sight::layout`] gives them the same texels every
    /// frame, and since #4440 nothing in a ribbon vertex is a function
    /// of the eye either, so the buffers already up there are still the
    /// right ones. At the shipped framing that is 88% of the drawing's
    /// points and the same share of its ribbons.
    ///
    /// What the eye still decides is one float per curve —
    /// [`ribbon::reference_depth`], packed here into a per-curve
    /// geometry the field rasterizes into its reference plane, at a
    /// hundred kilobytes against the eighteen megabytes of rails it
    /// stands in for.
    ///
    /// Returns false when the drawing does not fit the field — the
    /// layout refuses a curve past the scan's depth or a drawing past
    /// the canvas' texel count — which leaves the caller on its CPU
    /// path for that frame rather than showing a wrong picture.
    ///
    /// [`ribbon::reference_depth`]: crate::ribbon::reference_depth
    pub fn solve(
        &mut self,
        drawing: Drawing<'_>,
        eye: Vec3,
        view_proj: Mat4,
        bias: f32,
        aspect: f32,
        posing: Posing<'_>,
    ) -> bool {
        if self.disabled {
            return false;
        }
        let field = self.resolve_canvas(aspect);
        self.recanvas(field);
        let Ok(layout) = sight::layout(drawing, field) else {
            return false;
        };

        // The resident half packs against the sculpt and its rig; the
        // volatile half is re-solved on the CPU at whatever pose is
        // running and stands for itself.
        if mem::take(&mut self.resident_stale) {
            self.resident = Some(Staged {
                points: (
                    sight::point_vertices(drawing, layout.resident(), posing.bound),
                    sight::point_indices(layout.resident()),
                ),
                ribbons: stroke::ribbon_geometry(drawing, &layout, Half::Resident, posing.bound),
            });
        }

        self.solved = Some(Solved {
            points: (sight::posed_point_vertices(drawing, layout.volatile()), sight::point_indices(layout.volatile())),
            ribbons: stroke::ribbon_geometry(drawing, &layout, Half::Volatile, None),
            curves: (sight::curve_vertices(drawing, &layout, eye), sight::curve_indices(&layout)),
        });
        self.standing = Some(Standing {
            view_proj,
            eye,
            bias,
            bones: posing.bones,
            tone: posing.tone,
            occupied: layout.occupied(),
        });
        self.revision += 1;

        true
    }

    /// The reply to a requested register, in send order: the field's
    /// program then the ink's.
    pub fn registered(&mut self, result: Result<u32, ()>) {
        match result {
            Ok(program_id) => self.programs.answered(program_id, PROGRAM_COUNT),
            Err(()) => self.disabled = true,
        }
    }

    /// The reply to one requested texture create, in binding order.
    pub fn created(&mut self, result: Result<u32, ()>) {
        let Ok(texture_id) = result else {
            self.textures.clear();
            self.disabled = true;
            return;
        };
        self.textures.answered(texture_id, TEXTURE_COUNT);
    }

    /// The reply to one requested geometry create, in slot order:
    /// subject, the two point halves, the curves, the two ribbon
    /// halves.
    pub fn geometry_created(&mut self, result: Result<u32, ()>) {
        let Ok(geometry_id) = result else {
            self.geometries.clear();
            self.disabled = true;
            return;
        };

        self.geometries.answered(geometry_id, GEOMETRY_COUNT);
        if self.geometries.ready(GEOMETRY_COUNT) {
            self.subject = Resident::Live(self.geometries.ids[0]);
        }
    }

    /// The two register mails, once per session, sent the first frame
    /// there is a drawing to show. Both go out together — the replies
    /// come back in this order.
    pub fn take_registers(&mut self) -> Vec<ProgramRegister> {
        if self.disabled || self.programs.asking || self.programs.ready(PROGRAM_COUNT) || self.standing.is_none() {
            return Vec::new();
        }

        self.programs.asking = true;
        vec![sight::program(), stroke::program()]
    }

    /// The textures whose size no longer matches the window, released
    /// before the next creates.
    ///
    /// Their replacements arrive blank, so the planes stand for nothing
    /// until a dispatch fills them again — which is what clearing the
    /// dispatched revision says.
    pub fn take_destroys(&mut self) -> Vec<DestroyTexture> {
        if self.textures.asking || self.sized == self.canvas || self.sized.is_none() {
            return Vec::new();
        }

        self.sized = None;
        self.drawn = false;
        self.dispatched = None;
        let stale = mem::take(&mut self.textures.ids);
        stale.into_iter().map(|texture_id| DestroyTexture { texture_id }).collect()
    }

    /// The creates for one canvas size: five writable field planes at
    /// canvas resolution, the ink sheet at twice its edge, and the wash's
    /// ink coverage plane at the wash body's own extent.
    ///
    /// Every one is `Writable` — a program writes all seven — and the
    /// planes sample `Nearest` because a field texel is a point's
    /// verdict rather than a picture, while the sheet samples `Linear`
    /// so the composite down to the window resolves its supersample.
    pub fn take_creates(&mut self) -> Vec<CreateTexture> {
        if self.disabled || !self.programs.ready(PROGRAM_COUNT) || self.textures.asking || self.sized.is_some() {
            return Vec::new();
        }
        let Some((width, height)) = self.canvas else {
            return Vec::new();
        };

        self.textures.asking = true;
        self.sized = Some((width, height));
        let plane = CreateTexture {
            width,
            height,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        };
        let (sheet_width, sheet_height) = (width * stroke::SUPERSAMPLE, height * stroke::SUPERSAMPLE);
        let mut creates = vec![plane.clone(); sight::PLANE_COUNT];
        creates.push(CreateTexture {
            width: sheet_width,
            height: sheet_height,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        });
        // The wash body's own texels, which is what the coverage pass
        // resolves to against this program's doubled reference and what
        // the wash resolves to against the canvas. Both are floor
        // divisions of the same canvas, so the two extents are the same
        // number rather than two numbers that agree.
        creates.push(CreateTexture {
            width: (width / BODY_DIVISOR).max(1),
            height: (height / BODY_DIVISOR).max(1),
            ..plane
        });

        creates
    }

    /// The six geometry creates, once, in slot order. The subject's
    /// bytes and the resident half's have to be staged by then — a
    /// prepass with no subject occludes nothing, and a dispatch names
    /// one id per declared slot or warn-drops whole, so five of six
    /// buffers is no create at all.
    ///
    /// Nothing is taken until every buffer is present, because a take
    /// that then bailed would drop the subject's bytes on the floor and
    /// leave the prepass blind for the session.
    pub fn take_geometry_creates(&mut self) -> Vec<CreateGeometry> {
        if self.disabled || self.geometries.asking || self.geometries.ready(GEOMETRY_COUNT) {
            return Vec::new();
        }
        if self.subject_bytes.is_none() || self.resident.is_none() || self.solved.is_none() {
            return Vec::new();
        }
        let (subject, resident) = (self.subject_bytes.take(), self.resident.take());
        let (Some(subject), Some(resident), Some(solved)) = (subject, resident, self.solved.take()) else {
            return Vec::new();
        };

        self.geometries.asking = true;
        self.subject = Resident::Creating;
        let create = |layout: Vec<VertexAttribute>, packed: Packed| CreateGeometry {
            layout,
            vertices: packed.0,
            indices: packed.1,
        };
        vec![
            create(sight::subject_slot().layout, subject),
            create(sight::points_slot().layout, resident.points),
            create(sight::posed_points_slot().layout, solved.points),
            create(sight::curves_slot().layout, solved.curves),
            create(stroke::ribbon_slot().layout, resident.ribbons),
            create(stroke::posed_ribbon_slot().layout, solved.ribbons),
        ]
    }

    /// Every later frame's geometry, replacing the buffers the GPU holds
    /// in place.
    ///
    /// The subject and the resident half travel only when the subject
    /// changed. The volatile half and the per-curve references travel
    /// every frame the eye moved, which is what they were solved for —
    /// and between them they are the drawing's minority: 12% of its
    /// points at the shipped framing, the same share of its ribbons,
    /// and one float a curve.
    pub fn take_geometry_updates(&mut self) -> Vec<UpdateGeometry> {
        if !self.geometries.ready(GEOMETRY_COUNT) {
            return Vec::new();
        }
        let ids = &self.geometries.ids;

        let mut updates = Vec::new();
        let mut ship = |slot: usize, packed: Packed| {
            updates.push(UpdateGeometry { geometry_id: ids[slot], vertices: packed.0, indices: packed.1 });
        };
        if let Some(subject) = self.subject_bytes.take() {
            ship(sight::SUBJECT as usize, subject);
        }
        if let Some(resident) = self.resident.take() {
            ship(sight::RESIDENT as usize, resident.points);
            ship(RIBBONS[stroke::RESIDENT as usize], resident.ribbons);
        }
        // Moved out rather than cloned: the volatile drawing is
        // megabytes of vertex bytes at this scale and it is rebuilt
        // every frame the eye moves, so a copy here is a copy of it per
        // frame. The dispatch that follows needs only the uniforms.
        if let Some(solved) = self.solved.take() {
            ship(sight::VOLATILE as usize, solved.points);
            ship(sight::CURVES as usize, solved.curves);
            ship(RIBBONS[stroke::VOLATILE as usize], solved.ribbons);
        }

        updates
    }

    /// The two dispatches, field then ink, sent after the geometry they
    /// read. The field's planes are the ink's inputs, and a program's
    /// passes record in dispatch arrival order, so this order is what
    /// makes the ink read this frame's field rather than the last.
    ///
    /// Both or neither, and only when something they are a function of
    /// moved. The field is the drawing, the view and the extent; the ink
    /// is those planes and the same three. So one revision governs the
    /// pair, and a frame that matches the revision the standing planes
    /// were produced from ships nothing — a held camera pays the whole
    /// chain's 45 passes once and then nothing at all
    /// (iamacoffeepot/aether#4448). The uniform blobs are encoded here
    /// rather than at solve time because the extent is one of their
    /// fields and a resize changes it without a solve.
    pub fn take_dispatches(&mut self) -> Vec<ProgramDispatch> {
        if !self.textures.ready(TEXTURE_COUNT) || !self.geometries.ready(GEOMETRY_COUNT) {
            return Vec::new();
        }
        if self.subject.id().is_none() || self.dispatched == Some(self.revision) {
            return Vec::new();
        }
        let (Some(standing), Some(field)) = (self.standing, self.canvas) else {
            return Vec::new();
        };
        // A canvas the standing drawing outgrew. Held rather than
        // dispatched into, and held without recording the revision, so
        // the next solve — which re-lays the drawing against the new
        // extent, or refuses it by name — is what resolves this.
        if standing.occupied > field.0 as usize * field.1 as usize {
            return Vec::new();
        }
        let (programs, textures, geometries) = (&self.programs.ids, &self.textures.ids, &self.geometries.ids);

        self.drawn = true;
        self.dispatched = Some(self.revision);
        let Standing { view_proj, eye, bias, bones, tone, .. } = standing;
        vec![
            ProgramDispatch {
                program_id: programs[0],
                bindings: textures[..sight::PLANE_COUNT].to_vec(),
                geometries: geometries[..sight::GEOMETRY_COUNT].to_vec(),
                uniforms: sight::SightUniforms { view_proj, eye, field, bias, bones, tone }.encode(),
            },
            ProgramDispatch {
                program_id: programs[1],
                bindings: textures.clone(),
                geometries: RIBBONS.map(|slot| geometries[slot]).to_vec(),
                uniforms: stroke::StrokeUniforms { view_proj, eye, bias, field, bones }.encode(),
            },
        ]
    }

    /// The ink, standing in front of the wash and facing the eye.
    ///
    /// The same billboard the sheet uses, at a nearer standoff: it spans
    /// the frustum's cross-section exactly at its own depth, so the
    /// drawing — rasterized through this very matrix — projects back
    /// onto the window pixel for pixel whatever resolution the sheet was
    /// rendered at. Nothing needs the window in pixels, which is the
    /// reason this is a billboard rather than a screen rect.
    ///
    /// The material pass does not write depth, so the two rects do not
    /// fight: this one is sent after the sheet and blends over it.
    ///
    /// `Premultiplied` because the sheet is a render program's own
    /// output, and a fragment pass writing `Rgba8` onto a transparent
    /// clear stores colour already scaled by its coverage. Compositing
    /// it straight would weight it a second time and lay a quarter of
    /// the ink at a half-covered pixel — which, at one- to two-pixel
    /// strokes, is very nearly every inked pixel there is.
    #[must_use]
    pub fn draw(&self, view: &View, subject_radius: f32) -> Option<DrawMaterialTextured> {
        if !self.drawn {
            return None;
        }
        let texture_id = *self.textures.ids.get(SHEET)?;

        let forward = (view.target - view.eye).normalize_or(Vec3::new(0.0, 0.0, -1.0));
        // In front of the subject's near side, and so in front of the
        // sheet standing behind its far one. Floored well clear of the
        // near plane for a camera pushed inside the subject's radius.
        let depth = ((view.target - view.eye).length() - subject_radius - INK_STANDOFF).max(INK_DEPTH_FLOOR);
        let centre = view.eye + forward * depth;
        let right = forward.cross(Vec3::new(0.0, 1.0, 0.0)).normalize_or(Vec3::new(1.0, 0.0, 0.0));
        let up = right.cross(forward);

        let half_height = depth * (view.field_of_view * 0.5).tan();
        let half_width = half_height * view.aspect;
        let origin = centre - right * half_width - up * half_height;

        Some(DrawMaterialTextured {
            texture_id,
            blend: QuadBlend::Premultiplied,
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
                // The canvas' first row is the top of the view and the
                // rect's `v` runs up from its origin corner, so the
                // vertical axis flips here exactly as the sheet's does.
                u0: 0.0,
                v0: 1.0,
                u1: 1.0,
                v1: 0.0,
                tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deform::bone_uniform;
    use crate::extract::Settings;
    use crate::feature::{Curve3, FeatureClass, Pen, SurfacePoint};

    /// A subject for the prepass to have something to stage. Its shape
    /// is not the point — the drawing below is not extracted from it.
    const TRIANGLE: &[u8] = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";

    /// An unrigged subject at rest, which is what every case here is
    /// about — the questions below are the layer's cadence, not its
    /// posing.
    fn still<'a>() -> Posing<'a> {
        Posing { bound: None, bones: bone_uniform(&[]), tone: ToneUniforms::of(&Settings::default(), false) }
    }

    fn curve(seed: u64) -> Curve3 {
        Curve3 {
            points: (0..6)
                .map(|at| SurfacePoint::on_surface(Vec3::new(at as f32 * 0.05, 0.0, 0.0), Vec3::new(0.0, 0.0, 1.0)))
                .collect(),
            class: FeatureClass::Silhouette,
            pen: Pen::Ink,
            seed,
            authored: false,
        }
    }

    /// Everything mounted: the two programs registered, the six
    /// textures created at the announced canvas, the six geometries
    /// created — the state a layer reaches a few frames after the first
    /// solve, and the state every dispatch question below is asked from.
    ///
    /// The ids are the ask order, which is what the layer treats them
    /// as, so an id here is its own slot.
    fn mounted(strokes: &mut Strokes, mesh: &Mesh, drawing: Drawing<'_>, canvas: (u32, u32)) {
        strokes.resized(canvas.0, canvas.1);
        strokes.subject_changed(mesh, None);
        assert!(
            strokes.solve(drawing, Vec3::new(0.0, 0.0, 3.0), Mat4::IDENTITY, 0.01, 1.0, still()),
            "the first solve"
        );

        assert_eq!(strokes.take_registers().len(), PROGRAM_COUNT, "one register per program");
        for id in 0..PROGRAM_COUNT as u32 {
            strokes.registered(Ok(id));
        }
        assert!(strokes.take_destroys().is_empty(), "nothing to release before the first create");
        assert_eq!(strokes.take_creates().len(), TEXTURE_COUNT, "one create per plane, and the sheet");
        for id in 0..TEXTURE_COUNT as u32 {
            strokes.created(Ok(id));
        }
        assert_eq!(strokes.take_geometry_creates().len(), GEOMETRY_COUNT, "one create per declared slot");
        for id in 0..GEOMETRY_COUNT as u32 {
            strokes.geometry_created(Ok(id));
        }
    }

    /// The extent one dispatch's `SightParams` carries, read back out of
    /// the packed blob's first window.
    fn dispatched_field(dispatch: &ProgramDispatch) -> (u32, u32) {
        let lane = |at: usize| f32::from_le_bytes(dispatch.uniforms[at..at + 4].try_into().expect("four bytes"));

        (lane(80) as u32, lane(84) as u32)
    }

    /// Tripwire: a held view dispatches the field once and then not
    /// again.
    ///
    /// The field's chain is 45 passes over the canvas and its planes are
    /// a function of the drawing, the view and the extent — so a frame
    /// that moved none of them would spend the chain deriving the planes
    /// already standing in its own textures. The failure is invisible in
    /// the picture and shows only as a still camera costing what a
    /// turning one does, which is exactly how it went unnoticed long
    /// enough to be read off a pass table as a per-frame cost
    /// (iamacoffeepot/aether#4448).
    #[test]
    fn a_held_view_dispatches_the_field_once() {
        let mesh = Mesh::from_obj_bytes(TRIANGLE, 0).expect("one triangle parses");
        let (resident, volatile) = ([curve(1)], [curve(2)]);
        let drawing = Drawing { resident: &resident, volatile: &volatile };

        let mut strokes = Strokes::default();
        mounted(&mut strokes, &mesh, drawing, (64, 64));

        assert_eq!(strokes.take_dispatches().len(), 2, "the field and the ink, once the textures stand");
        for frame in 0..4 {
            assert!(strokes.take_dispatches().is_empty(), "held frame {frame} re-derived the standing planes");
        }
        assert!(strokes.draw(&held_view(), 1.0).is_some(), "the sheet still holds the ink it was dispatched");

        assert!(
            strokes.solve(drawing, Vec3::new(3.0, 1.0, -2.0), Mat4::IDENTITY, 0.01, 1.0, still()),
            "the solve after a turn"
        );
        assert_eq!(strokes.take_dispatches().len(), 2, "a turn re-derives both");
    }

    /// Tripwire: a resize re-fills the planes from the standing view,
    /// with no solve in between.
    ///
    /// A resize releases every texture in the set and creates fresh ones,
    /// and fresh ones are blank. The camera is very often held while a
    /// window is dragged, so nothing re-solves — and a layer that only
    /// dispatched off a solve would leave the ink standing on a blank
    /// field for as long as the camera stayed put, which is the whole
    /// class of bug a cadence has to not introduce. Nothing a point or a
    /// ribbon carries is a function of the canvas, so what this owes is
    /// the blob and a dispatch rather than a re-pack.
    #[test]
    fn a_resize_refills_the_planes_from_the_standing_view() {
        let mesh = Mesh::from_obj_bytes(TRIANGLE, 0).expect("one triangle parses");
        let (resident, volatile) = ([curve(1)], [curve(2)]);
        let drawing = Drawing { resident: &resident, volatile: &volatile };

        let mut strokes = Strokes::default();
        mounted(&mut strokes, &mesh, drawing, (64, 64));
        let first = strokes.take_dispatches();
        assert_eq!(dispatched_field(&first[0]), (64, 64), "the field the first dispatch was sized to");

        strokes.resized(48, 96);
        assert_eq!(strokes.take_destroys().len(), TEXTURE_COUNT, "every texture in the set follows the canvas");
        let creates = strokes.take_creates();
        assert_eq!((creates[0].width, creates[0].height), (48, 96), "the planes are re-created at the new canvas");
        for id in 0..TEXTURE_COUNT as u32 {
            strokes.created(Ok(TEXTURE_COUNT as u32 + id));
        }

        assert!(strokes.take_geometry_updates().is_empty(), "a resize re-packs nothing");
        let refilled = strokes.take_dispatches();
        assert_eq!(refilled.len(), 2, "the fresh planes are blank until something fills them");
        assert_eq!(dispatched_field(&refilled[0]), (48, 96), "and the blob carries the new extent");
        assert!(strokes.take_dispatches().is_empty(), "and only once");
    }

    /// Tripwire: the ink coverage plane must be created at the wash's own
    /// body extent, at a window big enough for the wash's clamp to bite.
    ///
    /// That plane is the one texture the two layers share — this layer
    /// writes it, the wash program binds and reads it (#4451) — and a
    /// program binding's size is checked against the extent its graph was
    /// registered at. So a canvas disagreement is not a slightly wrong
    /// picture: every wash dispatch is dropped whole in the cap, the sheet
    /// stays the transparent clear it was created as, and the frame shows
    /// ink on the raw background with no error raised anywhere.
    ///
    /// The frame order is the shipped one, and it is what made this
    /// survive: a desktop window announces its size *every* frame while a
    /// solve happens only when the eye moves, so on a still subject the
    /// announcement is the last writer of the canvas. A layer that
    /// resolved the window one way in `resized` and another in `solve`
    /// therefore stood on the `resized` answer forever — and a pose that
    /// moved re-solved every frame and hid it, which is how a still
    /// subject came to be the only one that never developed (#4465).
    #[test]
    fn a_window_past_the_wash_clamp_still_creates_the_plane_at_the_wash_body_extent() {
        let mesh = Mesh::from_obj_bytes(TRIANGLE, 0).expect("one triangle parses");
        let (resident, volatile) = ([curve(1)], [curve(2)]);
        let drawing = Drawing { resident: &resident, volatile: &volatile };
        // Past the 1280 long-edge ceiling, where the clamp actually bites;
        // inside it the two resolutions agree by accident.
        let window = (1600, 1200);

        let mut strokes = Strokes::default();
        strokes.resized(window.0, window.1);
        strokes.subject_changed(&mesh, None);
        assert!(strokes.solve(drawing, Vec3::new(0.0, 0.0, 3.0), Mat4::IDENTITY, 0.01, 4.0 / 3.0, still()));
        assert_eq!(strokes.take_registers().len(), PROGRAM_COUNT, "one register per program");
        for id in 0..PROGRAM_COUNT as u32 {
            strokes.registered(Ok(id));
        }

        // The frame after the solve, with the camera held: the window
        // re-announces the size it has always had.
        strokes.resized(window.0, window.1);
        let creates = strokes.take_creates();

        let (width, height) = wash_canvas(window.0, window.1).body();
        assert_eq!(
            (creates[INK_PLANE].width, creates[INK_PLANE].height),
            (width as u32, height as u32),
            "the wash binds this plane against the extent its graph declares",
        );
    }

    /// A view for [`Strokes::draw`] to place a billboard against. Its
    /// framing is not the point of any test here — only whether a sheet
    /// comes back at all.
    fn held_view() -> View {
        View {
            eye: Vec3::new(0.0, 0.0, 3.0),
            target: Vec3::new(0.0, 0.0, 0.0),
            view_proj: Mat4::IDENTITY,
            aspect: 1.0,
            field_of_view: 0.45,
        }
    }

    /// Tripwire: once the subject stands, a turn ships the volatile
    /// half and the per-curve references, and nothing else.
    ///
    /// This is what #4435 and #4440 bought between them, and it is
    /// invisible from the picture: a resident buffer that travelled
    /// every frame would render identically and cost the eighteen
    /// megabytes both issues were about. The converse fails silently
    /// too — a volatile buffer that stopped travelling draws the eye
    /// the drawing was first solved for, for the rest of the session.
    #[test]
    fn a_turn_ships_the_volatile_half_and_the_references() {
        let mesh = Mesh::from_obj_bytes(TRIANGLE, 0).expect("one triangle parses");
        let (resident, volatile) = ([curve(1)], [curve(2)]);
        let drawing = Drawing { resident: &resident, volatile: &volatile };
        let solve = |strokes: &mut Strokes, eye| strokes.solve(drawing, eye, Mat4::IDENTITY, 0.01, 1.0, still());

        let mut strokes = Strokes::default();
        strokes.resized(64, 64);
        strokes.subject_changed(&mesh, None);
        assert!(solve(&mut strokes, Vec3::new(0.0, 0.0, 3.0)), "the first solve");
        assert_eq!(strokes.take_geometry_creates().len(), GEOMETRY_COUNT, "one create per declared slot");
        for id in 0..GEOMETRY_COUNT as u32 {
            strokes.geometry_created(Ok(id));
        }

        assert!(solve(&mut strokes, Vec3::new(3.0, 1.0, -2.0)), "the solve after the turn");
        let travelled: Vec<u32> =
            strokes.take_geometry_updates().into_iter().map(|update| update.geometry_id).collect();

        // The ids answer in slot order, so an id here is its own slot.
        assert_eq!(
            travelled,
            vec![sight::VOLATILE, sight::CURVES, RIBBONS[stroke::VOLATILE as usize] as u32],
            "the subject, the resident points and the resident ribbons stay put",
        );
    }
}
