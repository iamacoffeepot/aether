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
pub mod obj;

use aether_math::Vec3;

use bvh::Bvh;

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
    bvh: Bvh,
}

/// A point where a level set crosses an edge.
#[derive(Clone, Copy)]
pub struct Crossing {
    pub pos: Vec3,
    pub normal: Vec3,
    /// An auxiliary field sampled at the crossing, when one was supplied.
    /// Creases use it to carry how steeply the relief was changing there.
    pub strength: f32,
}

impl Mesh {
    /// Build from OBJ bytes. No path: a wasm guest has no filesystem, so
    /// the bytes arrive by mail from `aether.fs` and the caller owns them.
    pub fn from_obj_bytes(bytes: &[u8], normal_relaxation: usize) -> Option<Self> {
        let raw = obj::parse(bytes);

        (!raw.faces.is_empty()).then(|| Self::build(raw.positions, raw.faces, normal_relaxation))
    }

    pub fn posed(&self, positions: Vec<Vec3>, normal_relaxation: usize) -> Self {
        Self::build(positions, self.faces.clone(), normal_relaxation)
    }

    fn build(positions: Vec<Vec3>, faces: Vec<[u32; 3]>, normal_relaxation: usize) -> Self {
        let raw = obj::Raw { positions, faces };
        let mut normals = vertex_normals(&raw.positions, &raw.faces);
        for _ in 0..normal_relaxation {
            normals = relax(&normals, &raw.faces);
        }

        let (min, max) = raw.positions.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| {
            (
                Vec3::new(lo.x.min(p.x), lo.y.min(p.y), lo.z.min(p.z)),
                Vec3::new(hi.x.max(p.x), hi.y.max(p.y), hi.z.max(p.z)),
            )
        });

        // Orientation check: for a closed surface the outward normal
        // agrees with the direction from the centroid. Near +1 is outward,
        // near -1 inward, near 0 means the winding is inconsistent and
        // every view-dependent feature will dissolve.
        let centre = (min + max) * 0.5;
        let agreement: f32 =
            raw.positions.iter().zip(&normals).map(|(&p, &n)| (p - centre).normalize_or(n).dot(n)).sum::<f32>()
                / raw.positions.len() as f32;
        tracing::debug!(target: "aether_puppet", agreement, "normal orientation");

        let bvh = Bvh::build(&raw.positions, &raw.faces);
        Self { positions: raw.positions, faces: raw.faces, normals, min, max, bvh }
    }

    pub fn centre(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    pub fn occluded(&self, origin: Vec3, dir: Vec3, distance: f32) -> bool {
        self.bvh.occluded(&self.positions, &self.faces, origin, dir, 1e-4, distance)
    }

    /// First surface along the ray: its point and interpolated normal.
    pub fn hit(&self, origin: Vec3, dir: Vec3) -> Option<Crossing> {
        let (t, face) = self.bvh.nearest(&self.positions, &self.faces, origin, dir, 1e-4, f32::MAX)?;
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
        let normal = (na * wa + nb * wb + nc * (1.0 - wa - wb).max(0.0)).normalize_or(na);

        Some(Crossing { pos, normal, strength: 0.0 })
    }

    /// Where an edge crosses `iso`, evaluated from the lower vertex index
    /// first — unconditionally, in both faces that share the edge.
    ///
    /// That ordering is the whole reason the welding stage works. Two
    /// faces meeting at an edge each emit an endpoint there, and if they
    /// interpolate in opposite directions the two results differ in the
    /// last bit and the seam never closes. Sorted, they are identical.
    fn crossing(&self, values: &[f32], aux: &[f32], iso: f32, a: u32, b: u32) -> Crossing {
        let (lo, hi) = if a < b {
            (a, b)
        } else {
            (b, a)
        };
        let (lo, hi) = (lo as usize, hi as usize);

        let span = values[hi] - values[lo];
        let t = if span.abs() < 1e-20 {
            0.5
        } else {
            (iso - values[lo]) / span
        };

        Crossing {
            pos: self.positions[lo].lerp(self.positions[hi], t),
            normal: self.normals[lo].lerp(self.normals[hi], t).normalize_or(self.normals[lo]),
            strength: match aux {
                [] => 0.0,
                field => field[lo] + (field[hi] - field[lo]) * t,
            },
        }
    }

    /// The segment, if any, where `iso` crosses one triangle.
    fn march(&self, face: usize, values: &[f32], aux: &[f32], iso: f32) -> Option<[Crossing; 2]> {
        let corners = self.faces[face];
        let above = corners.map(|c| values[c as usize] >= iso);
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
        let (a, b) = (corners[(odd + 1) % 3], corners[(odd + 2) % 3]);

        Some([self.crossing(values, aux, iso, corners[odd], a), self.crossing(values, aux, iso, corners[odd], b)])
    }

    /// Every segment of the single level set `values = iso`.
    pub fn level_set(&self, values: &[f32], aux: &[f32], iso: f32) -> Vec<[Crossing; 2]> {
        (0..self.faces.len()).filter_map(|f| self.march(f, values, aux, iso)).collect()
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
            .map(|(&p, &n)| (p - eye).normalize_or(Vec3::new(0.0, 0.0, 1.0)).dot(n))
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
        let total: f32 = self
            .faces
            .iter()
            .map(|f| {
                let [a, b, c] = f.map(|i| self.positions[i as usize]);
                ((b - a).length() + (c - b).length() + (a - c).length()) / 3.0
            })
            .sum();

        (total / self.faces.len() as f32).max(1e-9)
    }

    /// `p . axis` per vertex. Its level sets are parallel plane cuts.
    pub fn projected(&self, axis: Vec3) -> Vec<f32> {
        self.positions.iter().map(|p| p.dot(axis)).collect()
    }
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
