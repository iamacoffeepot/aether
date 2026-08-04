//! The material field, borrowed from the source project.
//!
//! Relief extraction finds every carved line on the sculpt, which is more
//! than a drawing wants — hair strand seams, ear fur and cloth folds all
//! qualify, and inked together they bury the face. The source pipeline
//! already solved "which part of the surface is this" as a material field
//! over a 256-cube lattice, so the drawing reads that answer rather than
//! inventing a geometric proxy for it: creases are kept where the label
//! says eye, brow or lips, and dropped everywhere else.
//!
//! The lattice is reconstructed from the mesh bounds by the same rule the
//! field was baked with — a cube over the longest axis plus 12% padding
//! either side — so no transform has to be carried alongside the volume.

use aether_math::Vec3;

/// Class indices as the source spike writes them: `0` unlabelled, then
/// `index + 1` into its material list.
pub const SKIN: u8 = 1;
pub const DRESS: u8 = 2;
pub const HAIR: u8 = 3;
pub const INNER_EAR: u8 = 4;
pub const TUFT: u8 = 5;
pub const LIPS: u8 = 6;
pub const BROW: u8 = 7;
pub const EYE: u8 = 8;

/// How many labelled classes the field carries, `SKIN..=EYE`.
pub const CLASSES: usize = EYE as usize;

/// The classification blur of the source pipeline (spike 139's
/// `MATERIAL_BLUR`), as a gaussian sigma in lattice cells.
///
/// The field is a hard voxel quantisation, and the surface it labels is
/// thinner than a cell in places — an ear is two sheets around one
/// labelled shell, so the nearest voxel to a point on the outer sheet is
/// as likely to say inner ear as hair. The source pipeline never reads
/// the nearest voxel: it blurs each class into an indicator volume and
/// argmaxes the blurred scores, so a thin shell's classification pools
/// over its neighbourhood and the sheet behind it gets a vote.
const SCORE_SIGMA: f32 = 1.2;

/// Gather radius for [`SCORE_SIGMA`], in cells. At 2.5 sigma the kernel
/// is under half a percent of its peak.
const SCORE_REACH: i32 = 3;

pub fn class_of(name: &str) -> Option<u8> {
    match name {
        "skin" => Some(SKIN),
        "dress" => Some(DRESS),
        "hair" => Some(HAIR),
        "inner_ear" => Some(INNER_EAR),
        "tuft" => Some(TUFT),
        "lips" => Some(LIPS),
        "brow" => Some(BROW),
        "eye" => Some(EYE),
        _ => None,
    }
}

pub struct Labels {
    cells: Vec<u8>,
    n: usize,
    origin: Vec3,
    spacing: f32,
}

impl Labels {
    /// Read a `uint8` C-order cubic `.npy` and place it against `min`/`max`,
    /// the bounds of the mesh it was baked from.
    ///
    /// Bytes rather than a path: a guest has no filesystem, so the field
    /// arrives by mail from `aether.fs` like the mesh does. `None` when the
    /// buffer is too short to carry a header or holds something that is not
    /// a cube — the lattice is reconstructed from the cell count, so a
    /// non-cube would silently place every sample in the wrong place.
    pub fn parse(bytes: &[u8], min: Vec3, max: Vec3, pad: f32) -> Option<Self> {
        // `\x93NUMPY`, major, minor, then a little-endian header length.
        let header_len = usize::from(u16::from_le_bytes([*bytes.get(8)?, *bytes.get(9)?]));
        let cells = bytes.get(10 + header_len..)?.to_vec();

        let n = (cells.len() as f64).cbrt().round() as usize;
        if n < 2 || n * n * n != cells.len() {
            return None;
        }

        let centre = (min + max) * 0.5;
        let span = (max - min).to_array().into_iter().fold(f32::MIN, f32::max);
        let origin = centre - Vec3::splat(span * (0.5 + pad));
        let spacing = span * (1.0 + 2.0 * pad) / (n - 1) as f32;

        Some(Self { cells, n, origin, spacing })
    }

    fn at(&self, ix: i32, iy: i32, iz: i32) -> u8 {
        let limit = self.n as i32;
        if ix < 0 || iy < 0 || iz < 0 || ix >= limit || iy >= limit || iz >= limit {
            return 0;
        }

        self.cells[(ix as usize * self.n + iy as usize) * self.n + iz as usize]
    }

    /// The class at a world point.
    ///
    /// The field is only labelled on a shell around the surface, and a
    /// vertex of the original reconstruction does not land exactly on the
    /// marched shell the labels were flooded over, so an unlabelled hit
    /// widens to the surrounding cells before giving up. Without that the
    /// mask is full of pinholes and the ink dashes along every crease.
    pub fn sample(&self, p: Vec3) -> u8 {
        let cell = (p - self.origin) / self.spacing;
        let (ix, iy, iz) = (cell.x.round() as i32, cell.y.round() as i32, cell.z.round() as i32);

        let direct = self.at(ix, iy, iz);
        if direct != 0 {
            return direct;
        }

        // Nearest labelled cell in the 3x3x3 neighbourhood, by ring.
        for radius in 1..=2 {
            let mut best = 0;
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    for dz in -radius..=radius {
                        let found = self.at(ix + dx, iy + dy, iz + dz);
                        if found != 0 {
                            best = found;
                        }
                    }
                }
            }
            if best != 0 {
                return best;
            }
        }

        0
    }

    pub fn is(&self, p: Vec3, wanted: &[u8]) -> bool {
        wanted.contains(&self.sample(p))
    }

    /// Every class's blurred indicator evaluated at `p` — the source
    /// pipeline's classification signal (spikes 139/142).
    ///
    /// Equivalent to gaussian-blurring each class's indicator volume at
    /// [`SCORE_SIGMA`] and reading the result at `p`, computed as a
    /// gather so no blurred volume is ever materialized. The consumer
    /// interpolates these scores across a face and argmaxes *after*
    /// interpolation — argmaxing here would reduce the whole exercise to
    /// nearest-neighbour with extra steps (spike 142's warning).
    pub fn class_scores(&self, p: Vec3) -> [f32; CLASSES] {
        let cell = (p - self.origin) / self.spacing;
        let (ix, iy, iz) = (cell.x.round() as i32, cell.y.round() as i32, cell.z.round() as i32);

        let mut scores = [0.0; CLASSES];
        for dx in -SCORE_REACH..=SCORE_REACH {
            for dy in -SCORE_REACH..=SCORE_REACH {
                for dz in -SCORE_REACH..=SCORE_REACH {
                    let found = self.at(ix + dx, iy + dy, iz + dz);
                    if found == 0 {
                        continue;
                    }

                    let offset =
                        Vec3::new((ix + dx) as f32 - cell.x, (iy + dy) as f32 - cell.y, (iz + dz) as f32 - cell.z);
                    scores[usize::from(found) - 1] += (-offset.dot(offset) / (2.0 * SCORE_SIGMA * SCORE_SIGMA)).exp();
                }
            }
        }

        scores
    }

    /// [`Self::class_scores`] over a vertex list, the per-vertex half of
    /// the source pipeline's per-pixel classification. Computed once per
    /// subject load and interpolated per pixel by the region rasterizer.
    pub fn vertex_scores(&self, positions: &[Vec3]) -> Vec<[f32; CLASSES]> {
        positions.iter().map(|&p| self.class_scores(p)).collect()
    }
}
