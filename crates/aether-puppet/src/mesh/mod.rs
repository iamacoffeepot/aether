//! The subject: a triangle mesh, and the two queries a line renderer
//! makes of one.
//!
//! **Level sets.** Give every vertex a scalar and ask where it crosses a
//! value, and you get a curve on the surface. That single operation
//! produces both of the drawing's main feature kinds — the silhouette is
//! the zero crossing of `view . normal`, and a hatch line is the crossing
//! of `position . axis` at a multiple of the spacing. Extracting the
//! silhouette this way rather than by collecting front/back-facing edge
//! pairs matters on a reconstruction this dense: an edge-walk follows the
//! jagged triangle boundary, while the interpolated crossing passes
//! smoothly through triangle interiors and gives a line worth inking.
//!
//! **Occlusion.** Whether anything stands between a point and the eye,
//! answered by the BVH.

pub mod bvh;

use aether_math::{Rigid, Vec3};

use crate::deform::Skin;
use bvh::{Bvh, SilhouetteBvh};

const FACING_BLOCK_SIZE: usize = 8192;
const FACING_RANGE_LIMIT: usize = 16384;

#[derive(Clone, Copy)]
struct FacingRange {
    start: usize,
    end: usize,
}

fn facing_ranges(faces: &[[u32; 3]]) -> Vec<FacingRange> {
    faces
        .chunks(FACING_BLOCK_SIZE)
        .map(|faces| {
            let mut vertices = faces.iter().flatten().copied();
            let first = vertices.next().expect("a face block is occupied");
            let (start, end) = vertices.fold((first, first), |(min, max), vertex| (min.min(vertex), max.max(vertex)));

            FacingRange { start: start as usize, end: end as usize + 1 }
        })
        .collect()
}

pub struct Mesh {
    pub positions: Vec<Vec3>,
    pub faces: Vec<[u32; 3]>,
    /// Area-weighted vertex normals, optionally relaxed. Shading and the
    /// silhouette both read these rather than face normals — the whole
    /// point is to treat the triangle soup as the smooth surface it is
    /// standing in for.
    pub normals: Vec<Vec3>,
    pub min: Vec3,
    pub max: Vec3,
    /// Absent on a posed copy, which is never asked a ray question — see
    /// [`Self::deformable`].
    bvh: Option<Bvh>,
    /// The shared hierarchy without its ray-only triangle cache, present
    /// only on a posed copy.
    silhouette: Option<SilhouetteBvh>,
    /// The exact bone maps used to write this posed copy.
    silhouette_transforms: Vec<Rigid>,
    facing_ranges: Vec<FacingRange>,
    /// Cached, because the occlusion bias reads it once per ray sample and
    /// the honest computation walks every face.
    mean_edge: f32,
}

/// Where on the surface a point was found: which face, and where inside
/// it.
///
/// This is what makes a pose cheap. A curve point is not a free-floating
/// position — it is an address on the sculpt, recorded when the level set
/// or the planting ray found it. Carrying the address means the point can
/// be re-evaluated against a posed surface for the cost of a barycentric
/// blend, instead of the level set being solved again
/// (iamacoffeepot/aether#4336).
#[derive(Clone, Copy, Debug)]
pub struct Anchorage {
    pub face: u32,
    /// Shares of the face's second and third corners; the first takes the
    /// remainder.
    pub u: f32,
    pub v: f32,
}

impl Anchorage {
    /// The three corner shares, in the face's own corner order.
    pub fn barycentric(self) -> [f32; 3] {
        [1.0 - self.u - self.v, self.u, self.v]
    }
}

/// A point where a level set crosses an edge.
#[derive(Clone, Copy)]
pub struct Crossing {
    pub pos: Vec3,
    pub normal: Vec3,
    /// An auxiliary field sampled at the crossing, when one was supplied.
    /// Creases use it to carry how steeply the relief was changing there.
    pub strength: f32,
    /// Where this crossing sits on the surface, so a pose can carry it.
    pub at: Anchorage,
}

impl Mesh {
    /// Build from OBJ bytes. No path: a wasm guest has no filesystem, so
    /// the bytes arrive by mail from `aether.fs` and the caller owns them.
    pub fn from_obj_bytes(bytes: &[u8], normal_relaxation: usize) -> Option<Self> {
        let raw = aether_mesh::parse_obj(bytes).ok()?;

        (!raw.faces.is_empty()).then(|| Self::build(raw.positions, raw.faces, normal_relaxation))
    }

    /// A copy of this mesh for a pose to be written into, sharing its
    /// silhouette topology and carrying no ray accelerator.
    ///
    /// The positions and normals arrive as the rest ones and are
    /// overwritten by [`Skin::pose_surface`]; [`Self::rebound`] closes the
    /// pose off. The ray cache stays absent because nothing asks the posed
    /// surface an occlusion question; only the topology and meaningful-bone
    /// bindings needed by silhouette extraction ride along.
    ///
    /// The mean edge length rides along rather than being re-measured. A
    /// bust rig is near-isometric, and this length sets the occlusion bias
    /// — a bias that jittered frame to frame with the pose would flicker
    /// self-occlusion along every long curve.
    ///
    /// [`Skin::pose_surface`]: crate::deform::Skin::pose_surface
    pub fn deformable(&self, skin: &Skin) -> Self {
        Self {
            positions: self.positions.clone(),
            faces: self.faces.clone(),
            normals: self.normals.clone(),
            min: self.min,
            max: self.max,
            bvh: None,
            silhouette: self.bvh.as_ref().map(|bvh| bvh.silhouette(skin, &self.faces)),
            silhouette_transforms: Vec::new(),
            facing_ranges: self.facing_ranges.clone(),
            mean_edge: self.mean_edge,
        }
    }

    /// Re-measure the bounds and retain the exact transforms after posing.
    pub fn rebound(&mut self, transforms: &[Rigid]) {
        (self.min, self.max) = bounds_of(&self.positions);
        self.silhouette_transforms.clear();
        self.silhouette_transforms.extend_from_slice(transforms);
    }

    /// Where an anchorage sits on *this* mesh.
    ///
    /// The address is the sculpt's, so asking it of the rest mesh and of a
    /// posed copy of it is how a point found on one is read on the other.
    pub fn at(&self, anchorage: Anchorage) -> Vec3 {
        self.faces[anchorage.face as usize]
            .iter()
            .zip(anchorage.barycentric())
            .fold(Vec3::ZERO, |sum, (&corner, share)| sum + self.positions[corner as usize] * share)
    }

    fn build(positions: Vec<Vec3>, faces: Vec<[u32; 3]>, normal_relaxation: usize) -> Self {
        let mut normals = vertex_normals(&positions, &faces);
        for _ in 0..normal_relaxation {
            normals = relax(&normals, &faces);
        }

        let (min, max) = bounds_of(&positions);

        // Orientation check: for a closed surface the outward normal
        // agrees with the direction from the centroid. Near +1 is outward,
        // near -1 inward, near 0 means the winding is inconsistent and
        // every view-dependent feature will dissolve.
        let centre = (min + max) * 0.5;
        let agreement: f32 =
            positions.iter().zip(&normals).map(|(&p, &n)| (p - centre).normalize_or(n).dot(n)).sum::<f32>()
                / positions.len() as f32;
        tracing::debug!(target: "aether_puppet", agreement, "normal orientation");

        let bvh = Some(Bvh::build(&positions, &normals, &faces));
        let facing_ranges = facing_ranges(&faces);
        let mean_edge = mean_edge_length(&positions, &faces);
        Self {
            positions,
            faces,
            normals,
            min,
            max,
            bvh,
            silhouette: None,
            silhouette_transforms: Vec::new(),
            facing_ranges,
            mean_edge,
        }
    }

    pub fn centre(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    pub fn occluded(&self, origin: Vec3, dir: Vec3, distance: f32) -> bool {
        self.bvh.as_ref().is_some_and(|bvh| bvh.occluded(origin, dir, 1e-4, distance))
    }

    /// First surface along the ray: its point and interpolated normal.
    pub fn hit(&self, origin: Vec3, dir: Vec3) -> Option<Crossing> {
        let (t, face) = self.bvh.as_ref()?.nearest(origin, dir, 1e-4, f32::MAX)?;
        let pos = origin + dir * t;

        // Barycentric blend of the vertex normals, so a mark lies on the
        // smooth surface the rest of the drawing is treating as real.
        let [a, b, c] = self.faces[face].map(|i| self.positions[i as usize]);
        let total = (b - a).cross(c - a).length();
        let (wa, wb) = if total < 1e-20 {
            (1.0, 0.0)
        } else {
            ((b - pos).cross(c - pos).length() / total, (c - pos).cross(a - pos).length() / total)
        };
        let [na, nb, nc] = self.faces[face].map(|i| self.normals[i as usize]);
        let wc = (1.0 - wa - wb).max(0.0);
        let normal = (na * wa + nb * wb + nc * wc).normalize_or(na);

        Some(Crossing { pos, normal, strength: 0.0, at: Anchorage { face: face as u32, u: wb, v: wc } })
    }

    /// Where an edge crosses `iso`, evaluated from the lower vertex index
    /// first — unconditionally, in both faces that share the edge.
    ///
    /// That ordering is the whole reason the welding stage works. Two
    /// faces meeting at an edge each emit an endpoint there, and if they
    /// interpolate in opposite directions the two results differ in the
    /// last bit and the seam never closes. Sorted, they are identical.
    /// `i` and `j` name the edge's two corners *within* `face`, so the
    /// crossing can report where inside the face it landed as well as
    /// where in the world — the [`Anchorage`] a pose carries it by.
    fn crossing_with(&self, face: usize, i: usize, j: usize, values: &[f32; 3], aux: &[f32], iso: f32) -> Crossing {
        let corners = self.faces[face];
        let (lo_corner, hi_corner) = if corners[i] < corners[j] {
            (i, j)
        } else {
            (j, i)
        };
        let (lo, hi) = (corners[lo_corner] as usize, corners[hi_corner] as usize);

        let span = values[hi_corner] - values[lo_corner];
        let t = if span.abs() < 1e-20 {
            0.5
        } else {
            (iso - values[lo_corner]) / span
        };

        let mut shares = [0.0f32; 3];
        shares[lo_corner] = 1.0 - t;
        shares[hi_corner] = t;

        Crossing {
            pos: self.positions[lo].lerp(self.positions[hi], t),
            normal: self.normals[lo].lerp(self.normals[hi], t).normalize_or(self.normals[lo]),
            strength: match aux {
                [] => 0.0,
                field => field[lo] + (field[hi] - field[lo]) * t,
            },
            at: Anchorage { face: face as u32, u: shares[1], v: shares[2] },
        }
    }

    /// The segment, if any, where `iso` crosses one triangle.
    fn march_with(&self, face: usize, value: &impl Fn(usize) -> f32, aux: &[f32], iso: f32) -> Option<[Crossing; 2]> {
        let corners = self.faces[face];
        let values = corners.map(|c| value(c as usize));
        self.march_values(face, values, aux, iso)
    }

    fn march_values(&self, face: usize, values: [f32; 3], aux: &[f32], iso: f32) -> Option<[Crossing; 2]> {
        let above = values.map(|value| value >= iso);
        if above[0] == above[1] && above[1] == above[2] {
            return None;
        }

        // Exactly one corner is on its own; the crossings are on the two
        // edges that touch it.
        let odd = match above {
            [x, y, _] if x == y => 2,
            [x, _, z] if x == z => 1,
            _ => 0,
        };
        let (a, b) = ((odd + 1) % 3, (odd + 2) % 3);

        Some([self.crossing_with(face, odd, a, &values, aux, iso), self.crossing_with(face, odd, b, &values, aux, iso)])
    }

    fn march(&self, face: usize, values: &[f32], aux: &[f32], iso: f32) -> Option<[Crossing; 2]> {
        self.march_with(face, &|vertex| values[vertex], aux, iso)
    }

    /// Every segment of the single level set `values = iso`.
    pub fn level_set(&self, values: &[f32], aux: &[f32], iso: f32) -> Vec<[Crossing; 2]> {
        (0..self.faces.len()).filter_map(|f| self.march(f, values, aux, iso)).collect()
    }

    /// The silhouette's exact zero crossings after conservative BVH
    /// pruning. Leaves use the same facing and crossing arithmetic as the
    /// linear oracle; sorting candidates restores original face order
    /// before welding observes them.
    pub(crate) fn silhouette_level_set(&self, eye: Vec3) -> Vec<[Crossing; 2]> {
        if let Some(silhouette) = &self.silhouette {
            let pruned = silhouette.faces(eye, &self.silhouette_transforms);
            return self.silhouette_level_set_faces((0..self.faces.len()).filter(|&face| !pruned.contains(face)), eye);
        }
        let faces = self.bvh.as_ref().map_or_else(|| (0..self.faces.len()).collect(), |bvh| bvh.silhouette_faces(eye));

        self.silhouette_level_set_faces(faces, eye)
    }

    fn silhouette_level_set_faces(&self, faces: impl IntoIterator<Item = usize>, eye: Vec3) -> Vec<[Crossing; 2]> {
        let mut segments = Vec::new();
        let mut values = Vec::new();
        let mut faces = faces.into_iter().peekable();
        while let Some(&face) = faces.peek() {
            let block_index = face / FACING_BLOCK_SIZE;
            let range = self.facing_ranges[block_index];
            let range_start = range.start;
            let range_end = range.end;
            if range_end - range_start <= FACING_RANGE_LIMIT {
                values.clear();
                values.extend(
                    self.positions[range_start..range_end]
                        .iter()
                        .zip(&self.normals[range_start..range_end])
                        .map(|(&position, &normal)| (position - eye).dot(normal)),
                );
                while faces.peek().is_some_and(|face| *face / FACING_BLOCK_SIZE == block_index) {
                    let face = faces.next().expect("the block still has a face");
                    let facing = self.faces[face].map(|vertex| values[vertex as usize - range_start]);
                    if let Some(segment) = self.march_values(face, facing, &[], 0.0) {
                        segments.push(segment);
                    }
                }
            } else {
                while faces.peek().is_some_and(|face| *face / FACING_BLOCK_SIZE == block_index) {
                    let face = faces.next().expect("the block still has a face");
                    let facing = self.faces[face]
                        .map(|vertex| (self.positions[vertex as usize] - eye).dot(self.normals[vertex as usize]));
                    if let Some(segment) = self.march_values(face, facing, &[], 0.0) {
                        segments.push(segment);
                    }
                }
            }
        }

        segments
    }

    /// Every level set of `values` at integer multiples of `spacing`,
    /// bucketed by multiple.
    ///
    /// One pass over the faces serves all of them: a triangle only spans
    /// the handful of planes between its own minimum and maximum, so the
    /// cost is the surface area divided by the spacing rather than the
    /// face count times the plane count.
    pub fn level_sets(&self, values: &[f32], spacing: f32) -> Vec<(i32, Vec<[Crossing; 2]>)> {
        let low = values.iter().copied().fold(f32::MAX, f32::min);
        let high = values.iter().copied().fold(f32::MIN, f32::max);
        let base = (low / spacing).floor() as i32;
        let mut buckets: Vec<Vec<[Crossing; 2]>> =
            vec![Vec::new(); ((high / spacing).ceil() as i32 - base + 1).max(1) as usize];

        for face in 0..self.faces.len() {
            let corners = self.faces[face].map(|c| values[c as usize]);
            let face_low = corners.iter().copied().fold(f32::MAX, f32::min);
            let face_high = corners.iter().copied().fold(f32::MIN, f32::max);

            for plane in (face_low / spacing).ceil() as i32..=(face_high / spacing).floor() as i32 {
                if let Some(segment) = self.march(face, values, &[], plane as f32 * spacing) {
                    buckets[(plane - base) as usize].push(segment);
                }
            }
        }

        buckets
            .into_iter()
            .enumerate()
            .filter(|(_, bucket)| !bucket.is_empty())
            .map(|(i, bucket)| (base + i as i32, bucket))
            .collect()
    }

    /// `normalize(p - eye) . n` per vertex. Its zero set is the silhouette.
    pub fn facing(&self, eye: Vec3) -> Vec<f32> {
        self.positions
            .iter()
            .zip(&self.normals)
            // Unnormalised. The silhouette is this field's *zero* set, and
            // scaling each vertex's value by its own positive distance to
            // the eye cannot move a sign — it only reweights where between
            // two vertices the crossing interpolates to, by well under a
            // pixel. The normalize it replaces was a square root per vertex,
            // 434k of them, every frame.
            .map(|(&p, &n)| (p - eye).dot(n))
            .collect()
    }

    /// Surface relief: how far the surface bulges or recedes at feature
    /// scale, per vertex.
    ///
    /// This is a band-pass on the surface. Smooth the positions twice, at
    /// a fine scale and a coarse one, and take the difference along the
    /// normal: the coarse pass removes the overall form and the fine pass
    /// removes the reconstruction noise, leaving exactly the scale of
    /// detail a sculptor carves — an eyelid fold, a lip line, the seam of
    /// a hair strand. Positive in a valley, negative on a ridge.
    ///
    /// Level sets of this field are the drawing's crease lines, which is
    /// why it exists: the same machinery that finds silhouettes and hatch
    /// finds the etched detail too, with no third extractor.
    pub fn relief(&self, fine: usize, coarse: usize) -> Vec<f32> {
        let near = smooth(&self.positions, &self.faces, fine);
        let far = smooth(&near, &self.faces, coarse.saturating_sub(fine));

        // Scaled by the mean edge length so the threshold means the same
        // thing on any mesh, however finely it happens to be tessellated.
        let scale = 1.0 / self.mean_edge_length();
        near.iter().zip(&far).zip(&self.normals).map(|((&a, &b), &n)| (b - a).dot(n) * scale).collect()
    }

    /// Magnitude of the surface gradient of a per-vertex field, made
    /// dimensionless by the mean edge length.
    ///
    /// This is what separates a carved crease from a gentle swell. Both
    /// cross a given relief value somewhere, but the crease crosses it
    /// steeply over a couple of edges while the swell drifts across it
    /// over half a cheek — so the swell's level set closes into a blotch
    /// that no illustrator would ink, and the gradient is the one number
    /// that tells them apart.
    pub fn gradient(&self, values: &[f32]) -> Vec<f32> {
        let mut sum = vec![0.0f32; self.positions.len()];
        let mut weight = vec![0.0f32; self.positions.len()];

        for face in &self.faces {
            let [a, b, c] = face.map(|i| self.positions[i as usize]);
            let [fa, fb, fc] = face.map(|i| values[i as usize]);

            let cross = (b - a).cross(c - a);
            let area2 = cross.length();
            if area2 < 1e-20 {
                continue;
            }

            let n = cross / area2;
            let grad = (n.cross(c - b) * fa + n.cross(a - c) * fb + n.cross(b - a) * fc).length() / area2;

            for &i in face {
                sum[i as usize] += grad * area2;
                weight[i as usize] += area2;
            }
        }

        let scale = self.mean_edge_length();
        sum.iter()
            .zip(&weight)
            .map(|(&s, &w)| {
                if w > 0.0 {
                    s / w * scale
                } else {
                    0.0
                }
            })
            .collect()
    }

    /// The same per-face gradient, kept as a vector.
    ///
    /// `gradient` throws the direction away because a crease only needs to
    /// know how fast relief changes. A suggestive contour needs to know
    /// which way, because the whole question it asks is about change along
    /// one particular direction.
    pub fn gradient_vector(&self, values: &[f32]) -> Vec<Vec3> {
        let mut sum = vec![Vec3::splat(0.0); self.positions.len()];
        let mut weight = vec![0.0f32; self.positions.len()];

        for face in &self.faces {
            let [a, b, c] = face.map(|i| self.positions[i as usize]);
            let [fa, fb, fc] = face.map(|i| values[i as usize]);

            let cross = (b - a).cross(c - a);
            let area2 = cross.length();
            if area2 < 1e-20 {
                continue;
            }

            let n = cross / area2;
            let grad = (n.cross(c - b) * fa + n.cross(a - c) * fb + n.cross(b - a) * fc) / area2;

            for &i in face {
                sum[i as usize] += grad * area2;
                weight[i as usize] += area2;
            }
        }

        sum.iter()
            .zip(&weight)
            .map(|(&s, &w)| {
                if w > 0.0 {
                    s / w
                } else {
                    Vec3::splat(0.0)
                }
            })
            .collect()
    }

    /// Suggestive contours: where the surface would become a silhouette
    /// under a small move of the eye.
    ///
    /// A silhouette is the zero set of `n . v`. This is the set where that
    /// same quantity bottoms out along the view direction projected into
    /// the surface — a minimum that never reaches zero. It is the line a
    /// nose makes seen head-on, and the reason it reads as one pen stroke
    /// rather than as a placed mark is that it joins the silhouette
    /// exactly: run the eye around and a suggestive contour walks to the
    /// profile edge and becomes it.
    ///
    /// `gate` keeps it near the turn. Without one the criterion fires on
    /// every gentle undulation across the whole figure, which is true and
    /// useless — an illustrator draws the ones about to become an edge.
    pub fn suggestive(&self, eye: Vec3, threshold: f32, gate: f32) -> Vec<[Crossing; 2]> {
        let toward: Vec<Vec3> =
            self.positions.iter().map(|&p| (eye - p).normalize_or(Vec3::new(0.0, 0.0, 1.0))).collect();
        let facing: Vec<f32> = toward.iter().zip(&self.normals).map(|(&v, &n)| v.dot(n)).collect();

        // The view direction flattened into the tangent plane: the one
        // direction along which "about to turn away" is even a question.
        let along: Vec<Vec3> = toward
            .iter()
            .zip(&self.normals)
            .map(|(&v, &n)| (v - n * v.dot(n)).normalize_or(Vec3::splat(0.0)))
            .collect();

        let slope = self.gradient_vector(&facing);
        let first: Vec<f32> = slope.iter().zip(&along).map(|(&g, &w)| g.dot(w)).collect();
        let bend = self.gradient_vector(&first);

        // Curvature carries the strength, gated to the front of the form
        // and to the band approaching the turn. Everything the gate zeroes
        // simply fails the threshold downstream.
        //
        // Scaled by the mean edge length squared, because this is a second
        // derivative and therefore lives in units of one-over-length-
        // squared: on a mesh of this density that is four orders of
        // magnitude, so an unscaled threshold is either everything or
        // nothing and there is no dial in between. Relief is made
        // dimensionless the same way, one derivative down.
        let scale = self.mean_edge_length().powi(2);
        let strength: Vec<f32> = bend
            .iter()
            .zip(&along)
            .zip(&facing)
            .map(|((&g, &w), &f)| {
                if f > 0.0 && f < gate {
                    g.dot(w) * scale
                } else {
                    0.0
                }
            })
            .collect();

        self.level_set(&first, &strength, 0.0)
            .into_iter()
            .filter(|[a, b]| a.strength.max(b.strength) >= threshold)
            .collect()
    }

    pub fn mean_edge_length(&self) -> f32 {
        self.mean_edge
    }

    /// How far to lift a ray off the surface before asking what occludes it.
    ///
    /// A fraction of an edge, not a constant. A level-set point sits *on* a
    /// triangle, and the smooth vertex normal it carries is not the flat
    /// face's, so a fixed epsilon that clears one mesh's triangles sinks
    /// under a coarser mesh's — the point then occludes itself, and every
    /// long curve comes back chopped into dashes. Scaling with the mesh
    /// means the same number works for a reconstruction and a decimation.
    pub fn surface_bias(&self) -> f32 {
        self.mean_edge * 0.9
    }

    /// `p . axis` per vertex. Its level sets are parallel plane cuts.
    pub fn projected(&self, axis: Vec3) -> Vec<f32> {
        self.positions.iter().map(|p| p.dot(axis)).collect()
    }
}

fn bounds_of(positions: &[Vec3]) -> (Vec3, Vec3) {
    positions.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| {
        (Vec3::new(lo.x.min(p.x), lo.y.min(p.y), lo.z.min(p.z)), Vec3::new(hi.x.max(p.x), hi.y.max(p.y), hi.z.max(p.z)))
    })
}

fn mean_edge_length(positions: &[Vec3], faces: &[[u32; 3]]) -> f32 {
    let total: f32 = faces
        .iter()
        .map(|f| {
            let [a, b, c] = f.map(|i| positions[i as usize]);
            ((b - a).length() + (c - b).length() + (a - c).length()) / 3.0
        })
        .sum();

    (total / faces.len().max(1) as f32).max(1e-9)
}

/// `iterations` umbrella passes over a vertex field.
fn smooth(values: &[Vec3], faces: &[[u32; 3]], iterations: usize) -> Vec<Vec3> {
    let mut current = values.to_vec();

    for _ in 0..iterations {
        let mut sum = vec![Vec3::splat(0.0); current.len()];
        let mut weight = vec![0.0f32; current.len()];

        for face in faces {
            for (i, &v) in face.iter().enumerate() {
                sum[v as usize] += current[face[(i + 1) % 3] as usize] + current[face[(i + 2) % 3] as usize];
                weight[v as usize] += 2.0;
            }
        }

        current = sum
            .iter()
            .zip(&weight)
            .zip(&current)
            .map(|((&s, &w), &c)| {
                if w > 0.0 {
                    s / w
                } else {
                    c
                }
            })
            .collect();
    }

    current
}

fn vertex_normals(positions: &[Vec3], faces: &[[u32; 3]]) -> Vec<Vec3> {
    let mut normals = vec![Vec3::splat(0.0); positions.len()];

    for face in faces {
        let [a, b, c] = face.map(|i| positions[i as usize]);
        // Un-normalised, so the accumulation is area-weighted for free.
        let n = (b - a).cross(c - a);
        for &i in face {
            normals[i as usize] += n;
        }
    }

    normals.iter().map(|&n| n.normalize_or(Vec3::new(0.0, 1.0, 0.0))).collect()
}

/// One umbrella smoothing pass over the normal field.
///
/// The subject is a photogrammetry-grade reconstruction, so its normals
/// carry high-frequency noise that a shaded render hides and a *line*
/// render does not: every wrinkle in the field becomes a spurious
/// silhouette loop. Relaxing the normals cleans the lines without moving
/// a single vertex, which keeps the geometry honest.
fn relax(normals: &[Vec3], faces: &[[u32; 3]]) -> Vec<Vec3> {
    let mut sum = normals.to_vec();

    for face in faces {
        for (i, &v) in face.iter().enumerate() {
            sum[v as usize] += normals[face[(i + 1) % 3] as usize] + normals[face[(i + 2) % 3] as usize];
        }
    }

    sum.iter().map(|&n| n.normalize_or(Vec3::new(0.0, 1.0, 0.0))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::deform::npy;
    use crate::feature::{Curve3, FeatureClass, Pen, SurfacePoint};
    use crate::weld;
    use crate::{Pose, extract};

    fn grid() -> Mesh {
        let mut positions = Vec::new();
        let mut faces = Vec::new();
        for row in 0..4 {
            for column in 0..4 {
                positions.push(Vec3::new(column as f32 - 1.5, row as f32 - 1.5, 1.0));
            }
        }
        let corner = |row: usize, column: usize| (row * 4 + column) as u32;
        for row in 0..3 {
            for column in 0..3 {
                faces.push([corner(row, column), corner(row, column + 1), corner(row + 1, column + 1)]);
                faces.push([corner(row, column), corner(row + 1, column + 1), corner(row + 1, column)]);
            }
        }

        Mesh::build(positions, faces, 0)
    }

    fn replace_normals(mesh: &mut Mesh, normals: Vec<Vec3>) {
        mesh.normals = normals;
        mesh.bvh = Some(Bvh::build(&mesh.positions, &mesh.normals, &mesh.faces));
    }

    fn assert_vec_bits(actual: Vec3, expected: Vec3) {
        assert_eq!(actual.x.to_bits(), expected.x.to_bits());
        assert_eq!(actual.y.to_bits(), expected.y.to_bits());
        assert_eq!(actual.z.to_bits(), expected.z.to_bits());
    }

    fn assert_crossings_identical(actual: &[[Crossing; 2]], expected: &[[Crossing; 2]]) {
        assert_eq!(actual.len(), expected.len(), "active face count");
        for (actual, expected) in actual.iter().flatten().zip(expected.iter().flatten()) {
            assert_vec_bits(actual.pos, expected.pos);
            assert_vec_bits(actual.normal, expected.normal);
            assert_eq!(actual.strength.to_bits(), expected.strength.to_bits());
            assert_eq!(actual.at.face, expected.at.face);
            assert_eq!(actual.at.u.to_bits(), expected.at.u.to_bits());
            assert_eq!(actual.at.v.to_bits(), expected.at.v.to_bits());
        }
    }

    fn oracle_curves(mesh: &Mesh, eye: Vec3) -> Vec<Curve3> {
        let segments = mesh
            .level_set(&mesh.facing(eye), &[], 0.0)
            .into_iter()
            .map(|crossings| crossings.map(|crossing| SurfacePoint::anchored(&crossing)))
            .collect();
        let template =
            Curve3 { points: Vec::new(), class: FeatureClass::Silhouette, pen: Pen::Ink, seed: 0, authored: false };

        weld::curves(segments, &template)
    }

    fn assert_curves_identical(actual: &[Curve3], expected: &[Curve3]) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.class, expected.class);
            assert_eq!(actual.pen, expected.pen);
            assert_eq!(actual.seed, expected.seed);
            assert_eq!(actual.authored, expected.authored);
            assert_eq!(actual.points.len(), expected.points.len());
            for (actual, expected) in actual.points.iter().zip(&expected.points) {
                assert_vec_bits(actual.pos, expected.pos);
                assert_vec_bits(actual.normal, expected.normal);
                assert_vec_bits(actual.probe, expected.probe);
                assert_eq!(actual.weight.to_bits(), expected.weight.to_bits());
                let (actual, expected) = (actual.anchorage.expect("extracted"), expected.anchorage.expect("extracted"));
                assert_eq!(actual.face, expected.face);
                assert_eq!(actual.u.to_bits(), expected.u.to_bits());
                assert_eq!(actual.v.to_bits(), expected.v.to_bits());
            }
        }
    }

    /// Exhaust every assignment of four repeated vertex-sign lanes over a
    /// tree large enough to split. This covers uniform prunes, mixed
    /// descent, exact zeros, and original-face restoration at the leaves.
    #[test]
    fn accelerated_silhouette_is_the_bit_exact_linear_level_set() {
        for signs in 0u32..16 {
            let mut mesh = grid();
            let vertices = mesh.positions.len();
            replace_normals(
                &mut mesh,
                (0..vertices)
                    .map(|vertex| {
                        if signs & (1 << (vertex % 4)) == 0 {
                            Vec3::Z
                        } else {
                            -Vec3::Z
                        }
                    })
                    .collect(),
            );

            for eye in [Vec3::ZERO, Vec3::new(0.0, 0.0, 1.0), Vec3::new(0.5, -0.5, 3.0)] {
                let expected = mesh.level_set(&mesh.facing(eye), &[], 0.0);
                let actual = mesh.silhouette_level_set(eye);
                assert_crossings_identical(&actual, &expected);
                assert_curves_identical(&extract::silhouettes(&mesh, eye), &oracle_curves(&mesh, eye));
            }
        }
    }

    /// Dense rest/pose eye coverage across identity, one-bone, and blended
    /// bindings. The posed accelerator sees the same transforms as the
    /// skin loop and must preserve crossings and welded stroke identity.
    #[test]
    fn posed_silhouette_matches_the_linear_oracle_across_eyes_and_bones() {
        let mut rest = grid();
        let normals = rest.positions.iter().map(|p| Vec3::new(p.x * 0.45, p.y * 0.35, 1.0).normalize()).collect();
        replace_normals(&mut rest, normals);

        let weights: Vec<f32> = (0..rest.positions.len())
            .flat_map(|vertex| match vertex % 3 {
                0 => [1.0, 0.0],
                1 => [0.0, 1.0],
                _ => [0.35, 0.65],
            })
            .collect();
        let skin = Skin::parse(
            &npy(&weights, (rest.positions.len(), 2)),
            "bones chest head\npivot head 0.0 0.0 0.0\n",
            rest.positions.len(),
        )
        .expect("two meaningful bone lanes");
        let transforms = skin.transforms(&Pose { yaw: 31.0, pitch: -11.0, roll: 7.0, ..Pose::default() });
        let mut posed = rest.deformable(&skin);
        skin.pose_surface(&transforms, &rest, &mut posed.positions, &mut posed.normals);
        posed.rebound(&transforms);

        for x in -3..=3 {
            for y in -2..=2 {
                for z in [0.0, 1.0, 2.5, 6.0] {
                    let eye = Vec3::new(x as f32 * 0.8, y as f32 * 0.7, z);
                    let expected = posed.level_set(&posed.facing(eye), &[], 0.0);
                    let actual = posed.silhouette_level_set(eye);
                    assert_eq!(
                        actual.iter().map(|segment| segment[0].at.face).collect::<Vec<_>>(),
                        expected.iter().map(|segment| segment[0].at.face).collect::<Vec<_>>(),
                        "candidate faces differ at eye {eye:?}",
                    );
                    assert_crossings_identical(&actual, &expected);
                    assert_curves_identical(&extract::silhouettes(&posed, eye), &oracle_curves(&posed, eye));
                }
            }
        }
    }

    /// Tripwire: the address a crossing records is the address the
    /// crossing is at.
    ///
    /// [`Anchorage`] is the whole basis of posing a drawing without
    /// re-extracting it, and the address is derived beside the position
    /// rather than from it — `crossing` interpolates along the edge from
    /// the lower *vertex index*, while the barycentric shares are written
    /// into the face's own *corner slots*, and the two orders agree only
    /// because the mapping between them is made explicitly. Get that
    /// mapping wrong and every crossing still lands in the right place,
    /// still reports a plausible address, and still draws an identical
    /// rest frame — the drawing only detaches from the surface once
    /// something poses it, a file away from the mistake.
    ///
    /// The strip's two faces name their corners in different orders and
    /// the level set cuts both, so a shares-to-slots mapping that happens
    /// to work for one of them is caught by the other.
    #[test]
    fn a_crossing_is_where_its_anchorage_says_it_is() {
        const OBJ: &[u8] = b"v -1 0 0\nv 1 0 0\nv -1 1 0\nv 1 1 0\nf 1 2 3\nf 2 4 3\n";
        let mesh = Mesh::from_obj_bytes(OBJ, 0).expect("a strip is a mesh");

        let heights: Vec<f32> = mesh.positions.iter().map(|p| p.y).collect();
        let segments = mesh.level_set(&heights, &[], 0.3);
        assert_eq!(segments.len(), 2, "the level set crosses both faces");

        for crossing in segments.iter().flatten() {
            let addressed = mesh.at(crossing.at);
            assert!(
                (crossing.pos - addressed).length() < 1e-6,
                "a crossing at {:?} addresses {addressed:?} on face {}",
                crossing.pos,
                crossing.at.face,
            );
        }
    }
}
