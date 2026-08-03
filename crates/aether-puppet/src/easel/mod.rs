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
//! camera; [`field`] turns a region into wet paint; [`palette`] owns the
//! pigments and composites the coats against paper white. This module is
//! the orchestrator: when to develop, what to upload, and where the sheet
//! stands.

pub mod accent;
pub mod field;
pub mod image;
pub mod palette;
pub mod regions;

use aether_math::{Mat4, Vec3};
use aether_render::{
    CreateTexture, DestroyTexture, DrawMaterialTextured, DrawTriangle, MaterialRect, MaterialTexturedRect,
    TextureFormat, TextureSampling, TextureUsage, UpdateTexture,
};

use crate::anchor::Anchors;
use crate::chart::{self, Face};
use crate::extract::Settings;
use crate::labels::Labels;
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
/// surface the wash bakes off, its material field, the drawing solved for
/// this eye (the flow's source), and the chart when the subject has a
/// face. Borrowed for the call — the easel keeps none of it.
pub struct Subject<'a> {
    pub mesh: &'a Mesh,
    pub labels: &'a Labels,
    pub settings: &'a Settings,
    pub drawn: &'a [DrawTriangle],
    pub chart: Option<Chart<'a>>,
}

/// The wash layer's state machine: a developed sheet waiting to upload, a
/// texture the render cap assigned, and the settle gate between paintings.
#[derive(Default)]
pub struct Easel {
    window: Option<(u32, u32)>,
    /// The eye on the previous render stage, for the settle gate: motion
    /// resets the count, stillness accumulates it.
    last_seen: Option<Vec3>,
    frames_still: u32,
    painted_from: Option<Vec3>,
    /// A developed sheet not yet on the GPU: width, height, RGBA8.
    pending: Option<(usize, usize, Vec<u8>)>,
    /// The registered texture and the size it was created at.
    texture: Option<(u32, (usize, usize))>,
    /// A create is in flight; hold further creates until it answers.
    creating: bool,
    /// The render cap refused a create — the headless chassis' fail-fast
    /// reply — so the easel stops asking rather than warn-storming.
    disabled: bool,
}

impl Easel {
    pub fn resized(&mut self, width: u32, height: u32) {
        self.window = Some((width, height));
    }

    /// A new subject or field arrived; the sheet no longer describes it.
    pub fn subject_changed(&mut self) {
        self.painted_from = None;
    }

    /// The reply to a requested create.
    pub fn created(&mut self, result: Result<u32, ()>) {
        self.creating = false;
        match result {
            Ok(texture_id) => {
                if let Some((width, height, _)) = self.pending.take() {
                    self.texture = Some((texture_id, (width, height)));
                }
            }
            Err(()) => self.disabled = true,
        }
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

    /// Re-develop the sheet if the view has settled somewhere unpainted.
    /// This is the slow path — a few hundred milliseconds of blurs — and
    /// the gate is what keeps it off every frame: never mid-drag, never
    /// twice for one view, never before the eye has rested.
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
        let Subject { mesh, labels, settings, drawn, chart } = subject;

        let regions = regions::rasterize(mesh, labels, settings, view.eye, &view.view_proj, width, height);
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
        let coats = sheet.coats(Some(&flow), accents.as_ref());
        self.pending = Some((width, height, palette::composite(&coats, sheet.paper_shade())));
        self.painted_from = Some(view.eye);
        self.frames_still = 0;
    }

    /// A texture whose size no longer matches the sheet, to release before
    /// the next create. Resize is the only path here.
    pub fn take_destroy(&mut self) -> Option<DestroyTexture> {
        let (_, created_at) = self.texture.as_ref()?;
        let (width, height, _) = self.pending.as_ref()?;
        if *created_at == (*width, *height) {
            return None;
        }

        let (texture_id, _) = self.texture.take()?;
        Some(DestroyTexture { texture_id })
    }

    /// The create carrying the first sheet at this size. The pixels ride
    /// the create itself; the pending sheet is dropped once the cap
    /// answers with an id.
    pub fn take_create(&mut self) -> Option<CreateTexture> {
        if self.disabled || self.creating || self.texture.is_some() {
            return None;
        }
        let (width, height, pixels) = self.pending.as_ref()?;

        self.creating = true;
        Some(CreateTexture {
            width: *width as u32,
            height: *height as u32,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: pixels.clone(),
        })
    }

    /// A freshly developed sheet over an existing same-size texture.
    pub fn take_update(&mut self) -> Option<UpdateTexture> {
        let (texture_id, created_at) = self.texture.as_ref()?;
        let (width, height, _) = self.pending.as_ref()?;
        if *created_at != (*width, *height) {
            return None;
        }

        let texture_id = *texture_id;
        let (width, height, pixels) = self.pending.take()?;
        Some(UpdateTexture { texture_id, x: 0, y: 0, width: width as u32, height: height as u32, pixels })
    }

    /// The sheet, standing behind the subject and facing the eye.
    ///
    /// It spans the view frustum's cross-section exactly at its depth, so
    /// the painting — developed through the same camera — projects back
    /// onto the window pixel for pixel and the wash lands in the ink's
    /// lines. Behind every ribbon by construction, the depth test keeps
    /// the drawing on top.
    pub fn draw(&self, view: &View, subject_radius: f32) -> Option<DrawMaterialTextured> {
        let (texture_id, _) = self.texture.as_ref()?;

        let forward = (view.target - view.eye).normalize_or(Vec3::new(0.0, 0.0, -1.0));
        let sheet_depth = (view.target - view.eye).length() + subject_radius + SHEET_STANDOFF;
        let centre = view.eye + forward * sheet_depth;
        let right = forward.cross(Vec3::new(0.0, 1.0, 0.0)).normalize_or(Vec3::new(1.0, 0.0, 0.0));
        let up = right.cross(forward);

        let half_height = sheet_depth * (view.field_of_view * 0.5).tan();
        let half_width = half_height * view.aspect;
        let origin = centre - right * half_width - up * half_height;

        Some(DrawMaterialTextured {
            texture_id: *texture_id,
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
