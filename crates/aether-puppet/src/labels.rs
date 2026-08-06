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
//! The on-disk `.npy` stays a compact authored asset. One decoder turns it
//! into a declared [`MaterialField`] kind carrying the lattice dimensions,
//! placement, cells, and class vocabulary that consumers otherwise had to
//! reconstruct by assumption.

use aether_math::Vec3;
use serde::{Deserialize, Serialize};

use crate::npy;

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

/// The canonical field's one-based class vocabulary. Cell `0` is unlabelled;
/// cell `n` names `CLASS_VOCABULARY[n - 1]`.
pub const CLASS_VOCABULARY: [&str; CLASSES] = ["skin", "dress", "hair", "inner_ear", "tuft", "lips", "brow", "eye"];

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

/// Gather radius for `SCORE_SIGMA`, in cells. At 2.5 sigma the kernel
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

/// A decoded material lattice with all interpretation declared in memory.
///
/// Cell order is C-order `(x, y, z)`: z varies fastest. `classes` is
/// one-based from the cells' point of view; cell `0` remains unlabelled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.material_field")]
pub struct MaterialField {
    pub dimensions: [u32; 3],
    #[serde(with = "aether_data::bytes")]
    pub cells: Vec<u8>,
    pub origin: [f32; 3],
    pub spacing: [f32; 3],
    pub classes: Vec<String>,
}

/// Compatibility name for the material-field consumer used throughout the
/// extraction pipeline.
pub type Labels = MaterialField;

impl MaterialField {
    /// Decode a `uint8` C-order cubic `.npy` and place it against `min`/`max`,
    /// the bounds of the mesh it was baked from, using the caller-declared
    /// bake padding.
    ///
    /// Bytes rather than a path: a guest has no filesystem, so the field
    /// arrives by mail from `aether.fs` like the mesh does. Only `NumPy` 1.0
    /// `|u1`, C-order arrays shaped exactly `(n, n, n)` with `n >= 2` are
    /// accepted; the diagnostic names any framing or metadata mismatch.
    pub fn decode(bytes: &[u8], min: Vec3, max: Vec3, padding: f32) -> Result<Self, String> {
        let array = npy::parse(bytes).map_err(|error| format!("material field refused: {error}"))?;
        if array.descr != "|u1" {
            return Err(format!("material field dtype is '{}', expected '|u1'", array.descr));
        }
        if array.fortran_order {
            return Err("material field is Fortran-order, expected C-order".to_owned());
        }
        let [nx, ny, nz] = array.shape.as_slice() else {
            return Err(format!("material field shape is {:?}, expected a cubic 3-D shape", array.shape));
        };
        if nx != ny || ny != nz || *nx < 2 {
            return Err(format!("material field shape is {:?}, expected (n, n, n) with n >= 2", array.shape));
        }
        let dimensions = [
            u32::try_from(*nx).map_err(|_| format!("material field x dimension {nx} exceeds u32"))?,
            u32::try_from(*ny).map_err(|_| format!("material field y dimension {ny} exceeds u32"))?,
            u32::try_from(*nz).map_err(|_| format!("material field z dimension {nz} exceeds u32"))?,
        ];
        if let Some((cell, &class)) =
            array.payload.iter().enumerate().find(|&(_, &class)| usize::from(class) > CLASS_VOCABULARY.len())
        {
            return Err(format!(
                "material field cell {cell} names class {class}, but the vocabulary has {} classes",
                CLASS_VOCABULARY.len(),
            ));
        }

        let mut field = Self {
            dimensions,
            cells: array.payload.to_vec(),
            origin: [0.0; 3],
            spacing: [1.0; 3],
            classes: CLASS_VOCABULARY.into_iter().map(str::to_owned).collect(),
        };
        field.place_against(min, max, padding);

        Ok(field)
    }

    /// Re-place the lattice against a mesh's bounds.
    ///
    /// The two loads settle in either order, and a field that lands
    /// before its mesh is first placed against stand-in bounds. Those
    /// bounds scale the whole lattice, so a 5% span error displaces a
    /// sample by cells — most at the crown, where the ear tips then read
    /// the concha's labels (issue 4401). Whoever holds the mesh calls
    /// this again once the real bounds are in.
    pub fn place_against(&mut self, min: Vec3, max: Vec3, padding: f32) {
        let centre = (min + max) * 0.5;
        let span = (max - min).to_array().into_iter().fold(f32::MIN, f32::max);
        let side = span * (1.0 + 2.0 * padding);

        self.origin = (centre - Vec3::splat(side * 0.5)).to_array();
        self.spacing = self.dimensions.map(|dimension| side / (dimension - 1) as f32);
    }

    fn at(&self, ix: i32, iy: i32, iz: i32) -> u8 {
        let [nx, ny, nz] = self.dimensions.map(i64::from);
        let (ix, iy, iz) = (i64::from(ix), i64::from(iy), i64::from(iz));
        if ix < 0 || iy < 0 || iz < 0 || ix >= nx || iy >= ny || iz >= nz {
            return 0;
        }

        self.cells[(ix as usize * ny as usize + iy as usize) * nz as usize + iz as usize]
    }

    fn lattice_position(&self, point: Vec3) -> Vec3 {
        let offset = point - Vec3::from_array(self.origin);
        Vec3::new(offset.x / self.spacing[0], offset.y / self.spacing[1], offset.z / self.spacing[2])
    }

    /// The class at a world point.
    ///
    /// The field is only labelled on a shell around the surface, and a
    /// vertex of the original reconstruction does not land exactly on the
    /// marched shell the labels were flooded over, so an unlabelled hit
    /// widens to the surrounding cells before giving up. Without that the
    /// mask is full of pinholes and the ink dashes along every crease.
    pub fn sample(&self, p: Vec3) -> u8 {
        let cell = self.lattice_position(p);
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
    /// `SCORE_SIGMA` and reading the result at `p`, computed as a
    /// gather so no blurred volume is ever materialized. The consumer
    /// interpolates these scores across a face and argmaxes *after*
    /// interpolation — argmaxing here would reduce the whole exercise to
    /// nearest-neighbour with extra steps (spike 142's warning).
    pub fn class_scores(&self, p: Vec3) -> [f32; CLASSES] {
        let cell = self.lattice_position(p);
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
