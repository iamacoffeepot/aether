//! Getting the drawing onto the GPU and back into the frame.
//!
//! The wash has the [`easel`](crate::easel) for this; the ink has this.
//! The two are shaped alike — register once, create per canvas size,
//! dispatch per repaint, present as a rect — and differ in cadence,
//! which is the whole reason they are separate state machines. The
//! easel develops when the view *stops* moving. The ink is re-solved
//! whenever the view moves at all, so everything here is sized for the
//! per-frame path: no settle gate, geometry replaced in place rather
//! than recreated, and the uniform blob rewritten every frame.
//!
//! Two programs run, in order, against one set of textures:
//!
//! 1. [`sight`] writes the four field planes at canvas resolution —
//!    the point's verdict, its reach, its curve's coverage, its run's
//!    arc.
//! 2. [`stroke`] reads those planes in its vertex stage and rasterizes
//!    the ribbons into a supersampled ink sheet.
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
};

use crate::easel::View;
use crate::easel::program::{sight, stroke};
use crate::feature::Curve3;
use crate::mesh::Mesh;

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

/// One frame's solved drawing, staged between the CPU solve and the
/// mail that carries it.
struct Solved {
    points: Vec<u8>,
    point_indices: Vec<u8>,
    ribbons: Vec<u8>,
    ribbon_indices: Vec<u8>,
    sight_uniforms: Vec<u8>,
    stroke_uniforms: Vec<u8>,
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
    /// The subject's packed bytes, waiting for the create or update.
    subject_bytes: Option<(Vec<u8>, Vec<u8>)>,
    solved: Option<Solved>,
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
/// The four field planes plus the ink sheet.
const TEXTURE_COUNT: usize = sight::PLANE_COUNT + 1;
/// Subject, points, ribbons.
const GEOMETRY_COUNT: usize = 3;

impl Strokes {
    /// A canvas change orphans every texture in the set — the field's
    /// capacity and the sheet's size both follow it.
    pub fn resized(&mut self, width: u32, height: u32) {
        self.window = Some((width, height));
    }

    /// The canvas to solve at.
    ///
    /// The window when one has been announced. Otherwise a stand-in of
    /// the same shape as the frustum, which is all the ink needs: it
    /// composites as a world-space billboard spanning the frustum's own
    /// cross-section, so the sheet's *resolution* is a quality choice
    /// and only its *aspect* has to agree with the view. That is the
    /// difference from a screen-space quad, which cannot be placed
    /// without knowing the window in pixels.
    fn resolve_canvas(&self, aspect: f32) -> (u32, u32) {
        if let Some(window) = self.window {
            return window;
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

    /// A new subject: its geometry has to travel again before the next
    /// prepass means anything.
    pub fn subject_changed(&mut self, mesh: &Mesh) {
        self.subject_bytes = Some((sight::subject_vertices(mesh), sight::subject_indices(mesh)));
    }

    /// Whether the layer can be asked to draw at all. A refused
    /// register (the headless chassis) leaves this false for the
    /// session, and the caller keeps its CPU path.
    #[must_use]
    pub fn live(&self) -> bool {
        !self.disabled
    }

    /// Solve one frame's drawing for the GPU: lay the field out over
    /// the curves, pack the points the field is rasterized from and the
    /// ribbons the ink is rasterized from, and stage both uniform
    /// blobs.
    ///
    /// Returns false when the drawing does not fit the field — the
    /// layout refuses a curve past the scan's depth or a drawing past
    /// the canvas' texel count — which leaves the caller on its CPU
    /// path for that frame rather than showing a wrong picture.
    pub fn solve(&mut self, curves: &[Curve3], eye: Vec3, view_proj: Mat4, bias: f32, aspect: f32) -> bool {
        if self.disabled {
            return false;
        }
        let field = self.resolve_canvas(aspect);
        self.canvas = Some(field);
        let Ok(layout) = sight::layout(curves, field) else {
            return false;
        };

        let (ribbons, ribbon_indices) = stroke::ribbon_geometry(curves, &layout, eye);
        self.solved = Some(Solved {
            points: sight::point_vertices(curves, &layout),
            point_indices: sight::point_indices(&layout),
            ribbons,
            ribbon_indices,
            sight_uniforms: sight::SightUniforms { view_proj, eye, field, bias }.encode(),
            stroke_uniforms: stroke::StrokeUniforms { view_proj, eye, bias, field }.encode(),
        });

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

    /// The reply to one requested geometry create: subject, points,
    /// ribbons.
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
        if self.disabled || self.programs.asking || self.programs.ready(PROGRAM_COUNT) || self.solved.is_none() {
            return Vec::new();
        }

        self.programs.asking = true;
        vec![sight::program(), stroke::program()]
    }

    /// The textures whose size no longer matches the window, released
    /// before the next creates.
    pub fn take_destroys(&mut self) -> Vec<DestroyTexture> {
        if self.textures.asking || self.sized == self.canvas || self.sized.is_none() {
            return Vec::new();
        }

        self.sized = None;
        self.drawn = false;
        let stale = mem::take(&mut self.textures.ids);
        stale.into_iter().map(|texture_id| DestroyTexture { texture_id }).collect()
    }

    /// The creates for one canvas size: four writable field planes at
    /// canvas resolution, then the ink sheet at twice its edge.
    ///
    /// Every one is `Writable` — a program writes all five — and the
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
        let mut creates = vec![plane; sight::PLANE_COUNT];
        creates.push(CreateTexture {
            width: sheet_width,
            height: sheet_height,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        });

        creates
    }

    /// The three geometry creates, once, in slot order. The subject's
    /// bytes have to be staged by then — a prepass with no subject
    /// occludes nothing.
    pub fn take_geometry_creates(&mut self) -> Vec<CreateGeometry> {
        if self.disabled || self.geometries.asking || self.geometries.ready(GEOMETRY_COUNT) {
            return Vec::new();
        }
        let (Some((vertices, indices)), Some(solved)) = (self.subject_bytes.take(), self.solved.as_mut()) else {
            return Vec::new();
        };

        self.geometries.asking = true;
        self.subject = Resident::Creating;
        vec![
            CreateGeometry { layout: sight::subject_slot().layout, vertices, indices },
            CreateGeometry {
                layout: sight::points_slot().layout,
                vertices: mem::take(&mut solved.points),
                indices: mem::take(&mut solved.point_indices),
            },
            CreateGeometry {
                layout: stroke::ribbon_slot().layout,
                vertices: mem::take(&mut solved.ribbons),
                indices: mem::take(&mut solved.ribbon_indices),
            },
        ]
    }

    /// Every later frame's geometry, replacing the resident bytes in
    /// place. The subject travels only when it changed; the points and
    /// the ribbons travel every frame the eye moved, which is what they
    /// were solved for.
    pub fn take_geometry_updates(&mut self) -> Vec<UpdateGeometry> {
        if !self.geometries.ready(GEOMETRY_COUNT) {
            return Vec::new();
        }
        let ids = &self.geometries.ids;

        let mut updates = Vec::new();
        if let Some((vertices, indices)) = self.subject_bytes.take() {
            updates.push(UpdateGeometry { geometry_id: ids[0], vertices, indices });
        }
        // Moved out rather than cloned: the drawing is megabytes of
        // vertex bytes at this scale and it is rebuilt every frame the
        // eye moves, so a copy here is a copy of the whole drawing per
        // frame. The dispatch that follows needs only the uniforms.
        // Skipped once the bytes have already travelled — the creates
        // move them out of the same staging, and shipping the emptied
        // vectors would replace the resident drawing with nothing.
        if let Some(solved) = self.solved.as_mut().filter(|solved| !solved.ribbons.is_empty()) {
            updates.push(UpdateGeometry {
                geometry_id: ids[1],
                vertices: mem::take(&mut solved.points),
                indices: mem::take(&mut solved.point_indices),
            });
            updates.push(UpdateGeometry {
                geometry_id: ids[2],
                vertices: mem::take(&mut solved.ribbons),
                indices: mem::take(&mut solved.ribbon_indices),
            });
        }

        updates
    }

    /// The two dispatches, field then ink, sent after the geometry they
    /// read. The field's planes are the ink's inputs, and a program's
    /// passes record in dispatch arrival order, so this order is what
    /// makes the ink read this frame's field rather than the last.
    pub fn take_dispatches(&mut self) -> Vec<ProgramDispatch> {
        if !self.textures.ready(TEXTURE_COUNT) || !self.geometries.ready(GEOMETRY_COUNT) {
            return Vec::new();
        }
        let (programs, textures, geometries) = (&self.programs.ids, &self.textures.ids, &self.geometries.ids);
        if self.subject.id().is_none() {
            return Vec::new();
        }
        let Some(solved) = self.solved.take() else {
            return Vec::new();
        };

        self.drawn = true;
        vec![
            ProgramDispatch {
                program_id: programs[0],
                bindings: textures[..sight::PLANE_COUNT].to_vec(),
                geometries: vec![geometries[0], geometries[1]],
                uniforms: solved.sight_uniforms,
            },
            ProgramDispatch {
                program_id: programs[1],
                bindings: textures.clone(),
                geometries: vec![geometries[2]],
                uniforms: solved.stroke_uniforms,
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
        let texture_id = *self.textures.ids.get(sight::PLANE_COUNT)?;

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
